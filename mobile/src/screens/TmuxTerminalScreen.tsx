import React, { useCallback, useEffect, useRef, useState } from 'react';
import { AppState, Text, TouchableOpacity, View, type AppStateStatus } from 'react-native';
import { WebView } from 'react-native-webview';
import {
  parseTmuxTerminalAttach,
  parseTmuxTerminalExit,
  parseTmuxTerminalOutput,
} from '../protocol';
import { registry } from '../runtime';
import { mapTmuxInputB64 } from '../tmuxCtrl';
import { isClosedTransportFailure } from '../transportError';
import { TMUX_TERMINAL_HTML } from '../tmuxTerminalHtml';
import { AppBar, styles, type Palette } from '../ui';

interface TerminalWebHandle {
  injectJavaScript: (script: string) => void;
}

interface TerminalWebProps {
  ref?: React.Ref<TerminalWebHandle | null>;
  originWhitelist: string[];
  source: { html: string; baseUrl?: string };
  onMessage: (event: { nativeEvent: { data: string } }) => void;
  javaScriptEnabled: boolean;
  mixedContentMode: 'always';
  setSupportMultipleWindows: boolean;
  allowFileAccess?: boolean;
  allowFileAccessFromFileURLs?: boolean;
  style: { flex: number; backgroundColor: string };
}

const TerminalWebView = WebView as unknown as React.ComponentType<TerminalWebProps>;

type HostMessage =
  | { type: 'ready'; cols: number; rows: number }
  | { type: 'input'; data: string }
  | { type: 'resize'; cols: number; rows: number };

function parseHostMessage(raw: string): HostMessage | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (!parsed || typeof parsed !== 'object') return null;
  const value = parsed as Record<string, unknown>;
  if (value.type === 'ready' || value.type === 'resize') {
    if (typeof value.cols !== 'number' || typeof value.rows !== 'number') return null;
    if (!Number.isInteger(value.cols) || !Number.isInteger(value.rows)) return null;
    return { type: value.type, cols: value.cols, rows: value.rows };
  }
  if (value.type === 'input' && typeof value.data === 'string') {
    return { type: 'input', data: value.data };
  }
  return null;
}

export function TmuxTerminalScreen(props: {
  palette: Palette;
  runnerId: string;
  session: string;
  onClose: () => void;
}): React.JSX.Element {
  const { palette } = props;
  const webRef = useRef<TerminalWebHandle>(null);
  const terminalIdRef = useRef<string | null>(null);
  const closedRef = useRef(false);
  const geometryRef = useRef<{ cols: number; rows: number } | null>(null);
  const attachRetryUsedRef = useRef(false);
  const [status, setStatus] = useState('');
  const [ctrlHeld, setCtrlHeld] = useState(false);
  const ctrlHeldRef = useRef(false);

  const leave = useCallback(() => {
    closedRef.current = true;
    props.onClose();
  }, [props.onClose]);

  const sendSoftKey = useCallback((seq: string) => {
    webRef.current?.injectJavaScript(`window.__rpiSoftKey && window.__rpiSoftKey(${JSON.stringify(seq)}); true;`);
  }, []);

  const pushOutput = useCallback((data: string) => {
    const script = `window.__rpiOutput && window.__rpiOutput(${JSON.stringify(data)}); true;`;
    webRef.current?.injectJavaScript(script);
  }, []);

  const detach = useCallback(async (terminalId: string): Promise<boolean> => {
    const client = registry.client(props.runnerId);
    if (!client) return false;
    try {
      if ((await client.command({ type: 'tmux_terminal_detach', terminalId })).success) return true;
    } catch (err) {
      if (isClosedTransportFailure(err)) {
        setStatus('Reconnecting');
        return false;
      }
    }
    try {
      return (await client.command({ type: 'tmux_terminal_detach', terminalId })).success;
    } catch (err) {
      if (isClosedTransportFailure(err)) setStatus('Reconnecting');
      return false;
    }
  }, [props.runnerId]);

  const runAttach = useCallback(async () => {
    const client = registry.client(props.runnerId);
    const geometry = geometryRef.current;
    if (!client || !geometry) return;
    try {
      const response = await client.command({
        type: 'tmux_terminal_attach',
        session: props.session,
        cols: geometry.cols,
        rows: geometry.rows,
      });
      const parsed = response.success ? parseTmuxTerminalAttach(response.data) : null;
      if (!parsed) {
        setStatus('Attach failed');
        return;
      }
      if (closedRef.current) {
        void detach(parsed.terminalId);
        return;
      }
      terminalIdRef.current = parsed.terminalId;
      attachRetryUsedRef.current = false;
      setStatus('');
    } catch (err) {
      if (isClosedTransportFailure(err)) {
        terminalIdRef.current = null;
        setStatus('Reconnecting');
        return;
      }
      setStatus(err instanceof Error ? err.message : 'Attach failed');
    }
  }, [detach, props.runnerId, props.session]);

  const noteClosedTransport = useCallback((err: unknown) => {
    if (!isClosedTransportFailure(err)) return;
    terminalIdRef.current = null;
    setStatus('Reconnecting');
  }, []);

  useEffect(() => {
    closedRef.current = false;
    attachRetryUsedRef.current = false;
    const client = registry.client(props.runnerId);
    if (!client) return;
    const off = client.subscribeFrames((frame) => {
      const terminalId = terminalIdRef.current;
      if (terminalId === null) return;
      const output = parseTmuxTerminalOutput(frame);
      if (output && output.terminalId === terminalId) {
        pushOutput(output.data);
        return;
      }
      const exit = parseTmuxTerminalExit(frame);
      if (exit && exit.terminalId === terminalId) setStatus('Session ended');
    });
    const offReconnect = client.subscribeReconnect(() => {
      if (closedRef.current) return;
      if (terminalIdRef.current !== null) return;
      if (attachRetryUsedRef.current) return;
      if (!geometryRef.current) return;
      attachRetryUsedRef.current = true;
      void runAttach();
    });
    return () => {
      closedRef.current = true;
      off();
      offReconnect();
      const terminalId = terminalIdRef.current;
      if (terminalId) void detach(terminalId);
    };
  }, [detach, props.runnerId, pushOutput, runAttach]);

  useEffect(() => {
    const sub = AppState.addEventListener('change', (next: AppStateStatus) => {
      if (next !== 'active' || closedRef.current) return;
      const client = registry.client(props.runnerId);
      if (!client) return;
      attachRetryUsedRef.current = false;
      if (!client.hasOpenSocket()) {
        terminalIdRef.current = null;
        setStatus('Reconnecting');
        return;
      }
      const terminalId = terminalIdRef.current;
      const geometry = geometryRef.current;
      if (terminalId === null || !geometry) return;
      void client.command({
        type: 'tmux_terminal_resize',
        terminalId,
        cols: geometry.cols,
        rows: geometry.rows,
      }).then((response) => {
        if (response.success || closedRef.current) return;
        terminalIdRef.current = null;
        setStatus('Reconnecting');
        void runAttach();
      }).catch(noteClosedTransport);
    });
    return () => sub.remove();
  }, [noteClosedTransport, props.runnerId, runAttach]);

  const onMessage = async (event: { nativeEvent: { data: string } }) => {
    const message = parseHostMessage(event.nativeEvent.data);
    if (!message) return;
    const client = registry.client(props.runnerId);
    if (!client) {
      setStatus('Runner is not connected.');
      return;
    }
    if (message.type === 'ready') {
      geometryRef.current = {
        cols: Math.max(8, message.cols),
        rows: Math.max(4, message.rows),
      };
      void runAttach();
      return;
    }
    const terminalId = terminalIdRef.current;
    if (terminalId === null) return;
    if (message.type === 'input') {
      let data = message.data;
      if (ctrlHeldRef.current) {
        try {
          const mapped = mapTmuxInputB64(data, true);
          if (mapped.consumed) {
            ctrlHeldRef.current = false;
            setCtrlHeld(false);
            data = mapped.data;
          }
        } catch {
          data = message.data;
        }
      }
      void client.command({ type: 'tmux_terminal_input', terminalId, data }).catch(noteClosedTransport);
      return;
    }
    void client.command({
      type: 'tmux_terminal_resize',
      terminalId,
      cols: message.cols,
      rows: message.rows,
    }).catch(noteClosedTransport);
  };

  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar
        palette={palette}
        title={props.session}
        onBack={leave}
        actions={[{ icon: 'close', label: 'Close', onPress: leave }]}
      />
      {status !== '' && (
        <Text
          style={[styles.inlineError, { color: status === 'Reconnecting' ? palette.muted : palette.error }]}
          accessibilityLabel={status}>{status}</Text>
      )}
      <TerminalWebView
        ref={webRef}
        originWhitelist={['*']}
        source={{ html: TMUX_TERMINAL_HTML, baseUrl: 'file:///android_asset/' }}
        onMessage={(event) => { void onMessage(event); }}
        javaScriptEnabled
        mixedContentMode="always"
        setSupportMultipleWindows={false}
        allowFileAccess
        allowFileAccessFromFileURLs
        style={{ flex: 1, backgroundColor: '#111111' }}
      />
      <View style={[styles.softKeyRow, { backgroundColor: palette.surface, borderTopColor: palette.border }]}>
        {([
          { label: 'Esc', seq: '\u001b' },
          { label: 'Ctrl', seq: '' },
          { label: 'Tab', seq: '\t' },
          { label: '↑', seq: '\u001b[A' },
          { label: '↓', seq: '\u001b[B' },
          { label: '←', seq: '\u001b[D' },
          { label: '→', seq: '\u001b[C' },
        ] as const).map((key) => (
          <TouchableOpacity
            key={key.label}
            style={[styles.softKey, { borderColor: palette.border, backgroundColor: key.label === 'Ctrl' && ctrlHeld ? palette.accent : palette.bg }]}
            onPress={() => {
              if (key.label === 'Ctrl') {
                setCtrlHeld((held) => {
                  const next = !held;
                  ctrlHeldRef.current = next;
                  return next;
                });
                return;
              }
              sendSoftKey(key.seq);
            }}
            accessibilityLabel={key.label}>
            <Text style={{ color: key.label === 'Ctrl' && ctrlHeld ? palette.bg : palette.fg, fontSize: 13 }}>{key.label}</Text>
          </TouchableOpacity>
        ))}
      </View>
    </View>
  );
}
