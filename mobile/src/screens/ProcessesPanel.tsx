import React, { useCallback, useEffect, useRef, useState } from 'react';
import { ScrollView, Text, TextInput, TouchableOpacity, View } from 'react-native';
import Icon from 'react-native-vector-icons/MaterialIcons';
import { Buffer } from 'buffer/';
import {
  decodeTmuxTerminalData,
  encodeTmuxTerminalData,
  parseProcessList,
  parseProcessLogs,
  parseRunnerState,
  type ProcessInfoData,
  type ProcessKey,
  type ProcessLogsData,
  type ProcessSignal,
} from '../protocol';
import { redactSecrets } from '../redact';
import type { RunnerClient } from '../runnerClient';
import { AppBar, styles, type Palette } from '../ui';

const LOG_LIMIT_BYTES = 16384;
const LOG_RENDER_CAP = 65536;
const WAIT_TIMEOUT_MS = 5000;
const DEFAULT_COLS = 80;
const DEFAULT_ROWS = 24;
const POLL_MS = 3000;

const PROCESS_KEYS: ProcessKey[] = [
  'ENTER', 'TAB', 'ESCAPE', 'CTRL_C', 'CTRL_D', 'UP', 'DOWN', 'LEFT', 'RIGHT',
];

const PROCESS_SIGNALS: ProcessSignal[] = [
  'SIGINT', 'SIGTERM', 'SIGHUP', 'SIGQUIT', 'SIGKILL',
];

function rpcError(prefix: string, error: unknown): string {
  if (typeof error === 'string' && error.trim() !== '') return `${prefix}: ${error}`;
  if (error instanceof Error && error.message.trim() !== '') return `${prefix}: ${error.message}`;
  return prefix;
}

function parseProcessInfo(data: unknown): ProcessInfoData | null {
  const rows = parseProcessList(Array.isArray(data) ? data : [data]);
  if (rows === null || rows.length === 0) return null;
  return rows[0];
}

function isLiveProcess(state: string): boolean {
  return state === 'starting' || state === 'running' || state === 'stopping';
}

function decodeLogPage(logs: ProcessLogsData): string | null {
  const parts: string[] = [];
  for (const chunk of logs.chunks) {
    const bytes = decodeTmuxTerminalData(chunk.dataBase64);
    if (bytes === null) return null;
    parts.push(Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength).toString('utf8'));
  }
  let text = redactSecrets(parts.join(''));
  if (logs.lost) text = `[lost ${logs.lostBytes} bytes]\n${text}`;
  return text;
}

function clipLogText(text: string): string {
  if (text.length <= LOG_RENDER_CAP) return text;
  return text.slice(text.length - LOG_RENDER_CAP);
}

const LABEL_MAX_BYTES = 64;

function parseBoundedInt(raw: string, whenEmpty: number, max: number): number | null {
  const trimmed = raw.trim();
  if (trimmed === '') return whenEmpty;
  if (!/^[0-9]+$/.test(trimmed)) return null;
  const value = Number(trimmed);
  if (!Number.isInteger(value) || value < 1 || value > max) return null;
  return value;
}

function processSpawnLabel(raw: string): string | null {
  const trimmed = raw.trim();
  if (trimmed === '') return '';
  if (trimmed.includes('\0')) return null;
  if (new TextEncoder().encode(trimmed).length > LABEL_MAX_BYTES) return null;
  return trimmed;
}

function frameSessionId(frame: unknown): string | null {
  if (!frame || typeof frame !== 'object') return null;
  const sessionId = (frame as Record<string, unknown>).sessionId;
  return typeof sessionId === 'string' && sessionId !== '' ? sessionId : null;
}

function processRowLabel(row: ProcessInfoData): string {
  const name = row.label !== null && row.label !== '' ? row.label : row.id;
  return redactSecrets(name);
}

function inputStyle(palette: Palette) {
  return [styles.input, { color: palette.fg, borderColor: palette.border, backgroundColor: palette.surface }];
}

function ghostButtonStyle(palette: Palette) {
  return [styles.primaryButton, { backgroundColor: palette.surface, borderWidth: 1, borderColor: palette.border }];
}

function SpawnForm(props: {
  palette: Palette;
  commandDraft: string;
  labelDraft: string;
  busy: boolean;
  canSpawn: boolean;
  onCommand: (value: string) => void;
  onLabel: (value: string) => void;
  onSpawn: () => void;
}): React.JSX.Element {
  const { palette } = props;
  return (
    <View style={[styles.form, { borderBottomWidth: 1, borderBottomColor: palette.border }]}>
      <Text style={[styles.formSection, { color: palette.muted }]}>New process</Text>
      <TextInput
        style={inputStyle(palette)}
        value={props.commandDraft}
        onChangeText={props.onCommand}
        placeholder="command"
        placeholderTextColor={palette.muted}
        editable={!props.busy}
        autoCapitalize="none"
        autoCorrect={false}
        accessibilityLabel="Command"
      />
      <TextInput
        style={inputStyle(palette)}
        value={props.labelDraft}
        onChangeText={props.onLabel}
        placeholder="label (optional)"
        placeholderTextColor={palette.muted}
        editable={!props.busy}
        autoCapitalize="none"
        autoCorrect={false}
        accessibilityLabel="Label"
      />
      <TouchableOpacity
        style={[styles.primaryButton, { backgroundColor: palette.accent }, !props.canSpawn && styles.faded]}
        disabled={!props.canSpawn}
        onPress={props.onSpawn}
        accessibilityLabel="Spawn">
        <Icon name="play-arrow" size={20} color={palette.bg} />
        <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Spawn</Text>
      </TouchableOpacity>
    </View>
  );
}

function ProcessRow(props: {
  palette: Palette;
  row: ProcessInfoData;
  onOpen: (processId: string) => void;
}): React.JSX.Element {
  const { palette, row } = props;
  const title = processRowLabel(row);
  return (
    <TouchableOpacity
      style={[styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border }]}
      onPress={() => props.onOpen(row.id)}
      accessibilityLabel={`Process ${title}`}>
      <View style={styles.rowBody}>
        <Text style={[styles.rowTitle, { color: palette.fg }]} numberOfLines={1}>{title}</Text>
        <Text style={[styles.rowSub, { color: palette.muted }]} numberOfLines={1}>
          {row.state}{row.pid !== null ? ` · ${row.pid}` : ''}
          {row.exitCode !== null ? ` · exit ${row.exitCode}` : ''}
        </Text>
      </View>
    </TouchableOpacity>
  );
}

function LiveProcessControls(props: {
  palette: Palette;
  busy: boolean;
  inputDraft: string;
  colsDraft: string;
  rowsDraft: string;
  onInput: (value: string) => void;
  onSend: () => void;
  onKey: (key: ProcessKey) => void;
  onCols: (value: string) => void;
  onRows: (value: string) => void;
  onResize: () => void;
  onStop: () => void;
  onSignal: (signal: ProcessSignal) => void;
}): React.JSX.Element {
  const { palette } = props;
  return (
    <>
      <TextInput
        style={inputStyle(palette)}
        value={props.inputDraft}
        onChangeText={props.onInput}
        placeholder="stdin"
        placeholderTextColor={palette.muted}
        editable={!props.busy}
        autoCapitalize="none"
        autoCorrect={false}
        accessibilityLabel="Process input"
      />
      <TouchableOpacity
        style={[styles.primaryButton, { backgroundColor: palette.accent }, (props.busy || props.inputDraft === '') && styles.faded]}
        disabled={props.busy || props.inputDraft === ''}
        onPress={props.onSend}
        accessibilityLabel="Send input">
        <Icon name="keyboard" size={20} color={palette.bg} />
        <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Send input</Text>
      </TouchableOpacity>
      <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: 8 }}>
        {PROCESS_KEYS.map((key) => (
          <TouchableOpacity
            key={key}
            style={[styles.primaryButton, { marginTop: 0, paddingVertical: 10, backgroundColor: palette.surface, borderWidth: 1, borderColor: palette.border }]}
            disabled={props.busy}
            onPress={() => props.onKey(key)}
            accessibilityLabel={`Key ${key}`}>
            <Text style={[styles.primaryButtonText, { color: palette.fg }]}>{key}</Text>
          </TouchableOpacity>
        ))}
      </View>
      <TextInput
        style={inputStyle(palette)}
        value={props.colsDraft}
        onChangeText={props.onCols}
        keyboardType="number-pad"
        editable={!props.busy}
        accessibilityLabel="Columns"
      />
      <TextInput
        style={inputStyle(palette)}
        value={props.rowsDraft}
        onChangeText={props.onRows}
        keyboardType="number-pad"
        editable={!props.busy}
        accessibilityLabel="Rows"
      />
      <TouchableOpacity style={ghostButtonStyle(palette)} disabled={props.busy} onPress={props.onResize} accessibilityLabel="Resize">
        <Icon name="aspect-ratio" size={20} color={palette.fg} />
        <Text style={[styles.primaryButtonText, { color: palette.fg }]}>Resize</Text>
      </TouchableOpacity>
      <TouchableOpacity
        style={[styles.primaryButton, { backgroundColor: palette.error }]}
        disabled={props.busy}
        onPress={props.onStop}
        accessibilityLabel="Stop">
        <Text style={[styles.primaryButtonText, { color: palette.bg }]}>■ Stop</Text>
      </TouchableOpacity>
      <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: 8 }}>
        {PROCESS_SIGNALS.map((signal) => (
          <TouchableOpacity
            key={signal}
            style={[styles.primaryButton, { marginTop: 0, paddingVertical: 10, backgroundColor: palette.surface, borderWidth: 1, borderColor: palette.border }]}
            disabled={props.busy}
            onPress={() => props.onSignal(signal)}
            accessibilityLabel={`Signal ${signal}`}>
            <Text style={[styles.primaryButtonText, { color: palette.fg }]}>{signal}</Text>
          </TouchableOpacity>
        ))}
      </View>
    </>
  );
}

function ProcessDetailPane(props: {
  palette: Palette;
  selected: ProcessInfoData;
  logText: string;
  logEof: boolean;
  live: boolean;
  busy: boolean;
  inputDraft: string;
  colsDraft: string;
  rowsDraft: string;
  waitDraft: string;
  onLoadLogs: () => void;
  onInput: (value: string) => void;
  onSend: () => void;
  onKey: (key: ProcessKey) => void;
  onCols: (value: string) => void;
  onRows: (value: string) => void;
  onResize: () => void;
  onStop: () => void;
  onSignal: (signal: ProcessSignal) => void;
  onWaitDraft: (value: string) => void;
  onWait: () => void;
}): React.JSX.Element {
  const { palette, selected } = props;
  return (
    <View style={styles.form}>
      <Text style={[styles.rowTitle, { color: palette.fg }]}>{processRowLabel(selected)}</Text>
      <Text style={[styles.rowSub, { color: palette.muted }]}>
        {selected.state}
        {selected.pid !== null ? ` · pid ${selected.pid}` : ''}
        {selected.exitCode !== null ? ` · exit ${selected.exitCode}` : ''}
      </Text>
      <Text
        style={[styles.rowSub, { color: palette.fg, fontFamily: palette.mono }]}
        accessibilityLabel="Process logs">
        {props.logText === '' ? '--' : props.logText}
      </Text>
      {!props.logEof && (
        <TouchableOpacity style={ghostButtonStyle(palette)} disabled={props.busy} onPress={props.onLoadLogs} accessibilityLabel="Load logs">
          <Icon name="subject" size={20} color={palette.fg} />
          <Text style={[styles.primaryButtonText, { color: palette.fg }]}>Load logs</Text>
        </TouchableOpacity>
      )}
      {props.live && (
        <LiveProcessControls
          palette={palette}
          busy={props.busy}
          inputDraft={props.inputDraft}
          colsDraft={props.colsDraft}
          rowsDraft={props.rowsDraft}
          onInput={props.onInput}
          onSend={props.onSend}
          onKey={props.onKey}
          onCols={props.onCols}
          onRows={props.onRows}
          onResize={props.onResize}
          onStop={props.onStop}
          onSignal={props.onSignal}
        />
      )}
      <TextInput
        style={inputStyle(palette)}
        value={props.waitDraft}
        onChangeText={props.onWaitDraft}
        keyboardType="number-pad"
        editable={!props.busy}
        accessibilityLabel="Wait timeout"
      />
      <TouchableOpacity style={ghostButtonStyle(palette)} disabled={props.busy} onPress={props.onWait} accessibilityLabel="Wait">
        <Icon name="hourglass-empty" size={20} color={palette.fg} />
        <Text style={[styles.primaryButtonText, { color: palette.fg }]}>Wait</Text>
      </TouchableOpacity>
    </View>
  );
}

export function ProcessesPanel(props: {
  palette: Palette;
  client: RunnerClient;
  sessionId: string;
  onClose: () => void;
  onError: (message: string) => void;
  onRegisterBackHandler?: (handler: (() => boolean) | null) => void;
}): React.JSX.Element {
  const { palette } = props;
  const [processes, setProcesses] = useState<ProcessInfoData[] | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [detail, setDetail] = useState<ProcessInfoData | null>(null);
  const [logText, setLogText] = useState('');
  const [logCursor, setLogCursor] = useState(0);
  const [logEof, setLogEof] = useState(false);
  const [spawnCwd, setSpawnCwd] = useState<string | null>(null);
  const [commandDraft, setCommandDraft] = useState('');
  const [labelDraft, setLabelDraft] = useState('');
  const [inputDraft, setInputDraft] = useState('');
  const [colsDraft, setColsDraft] = useState(String(DEFAULT_COLS));
  const [rowsDraft, setRowsDraft] = useState(String(DEFAULT_ROWS));
  const [waitDraft, setWaitDraft] = useState(String(WAIT_TIMEOUT_MS));
  const [busy, setBusy] = useState(false);

  const listGen = useRef(0);
  const logsGen = useRef(0);
  const detailGen = useRef(0);
  const cwdGen = useRef(0);
  const actionGen = useRef(0);
  const actionHeld = useRef(false);
  const selectedIdRef = useRef<string | null>(null);
  const logCursorRef = useRef(0);
  selectedIdRef.current = selectedId;
  logCursorRef.current = logCursor;

  const reportError = useCallback((message: string) => {
    props.onError(message);
  }, [props.onError]);

  const loadCwd = useCallback(async () => {
    const gen = ++cwdGen.current;
    try {
      const response = await props.client.command({ type: 'get_state' }, props.sessionId);
      if (gen !== cwdGen.current) return;
      if (!response.success) {
        reportError(rpcError('get_state failed', response.error));
        return;
      }
      const state = parseRunnerState(response.data);
      if (state === null) {
        reportError('Invalid get_state response');
        return;
      }
      if (state.sessionId !== props.sessionId) return;
      const cwd = state.cwd.trim();
      if (cwd === '') return;
      setSpawnCwd(cwd);
    } catch (error) {
      if (gen !== cwdGen.current) return;
      reportError(rpcError('get_state failed', error));
    }
  }, [props.client, props.sessionId, reportError]);

  const loadList = useCallback(async () => {
    const gen = ++listGen.current;
    try {
      const response = await props.client.command({ type: 'process_list' }, props.sessionId);
      if (gen !== listGen.current) return;
      if (!response.success) {
        reportError(rpcError('process_list failed', response.error));
        return;
      }
      const rows = parseProcessList(response.data);
      if (rows === null) {
        reportError('Invalid process_list response');
        return;
      }
      setProcesses(rows);
      const openId = selectedIdRef.current;
      if (openId !== null && !rows.some((row) => row.id === openId)) {
        selectedIdRef.current = null;
        setSelectedId(null);
        setDetail(null);
      }
    } catch (error) {
      if (gen !== listGen.current) return;
      reportError(rpcError('process_list failed', error));
    }
  }, [props.client, props.sessionId, reportError]);

  const loadLogs = useCallback(async (processId: string, cursor: number, append: boolean) => {
    const gen = ++logsGen.current;
    try {
      const response = await props.client.command({
        type: 'process_logs',
        processId,
        cursor,
        limitBytes: LOG_LIMIT_BYTES,
      }, props.sessionId);
      if (gen !== logsGen.current || selectedIdRef.current !== processId) return;
      if (!response.success) {
        reportError(rpcError('process_logs failed', response.error));
        return;
      }
      const page = parseProcessLogs(response.data);
      if (page === null) {
        reportError('Invalid process_logs response');
        return;
      }
      const decoded = decodeLogPage(page);
      if (decoded === null) {
        reportError('Invalid process_logs chunk');
        return;
      }
      setLogCursor(page.cursor);
      setLogEof(page.eof);
      setLogText((current) => clipLogText(append ? current + decoded : decoded));
    } catch (error) {
      if (gen !== logsGen.current) return;
      reportError(rpcError('process_logs failed', error));
    }
  }, [props.client, props.sessionId, reportError]);

  const loadDetail = useCallback(async (processId: string) => {
    const gen = ++detailGen.current;
    try {
      const response = await props.client.command({ type: 'process_describe', processId }, props.sessionId);
      if (gen !== detailGen.current || selectedIdRef.current !== processId) return;
      if (!response.success) {
        reportError(rpcError('process_describe failed', response.error));
        return;
      }
      const row = parseProcessInfo(response.data);
      if (row === null) {
        reportError('Invalid process_describe response');
        return;
      }
      setDetail(row);
    } catch (error) {
      if (gen !== detailGen.current) return;
      reportError(rpcError('process_describe failed', error));
    }
  }, [props.client, props.sessionId, reportError]);

  const openProcess = useCallback((processId: string) => {
    selectedIdRef.current = processId;
    setSelectedId(processId);
    setDetail(null);
    setLogText('');
    setLogCursor(0);
    setLogEof(false);
    void loadDetail(processId);
    void loadLogs(processId, 0, false);
  }, [loadDetail, loadLogs]);

  const runAction = useCallback((work: () => Promise<void>) => {
    if (actionHeld.current) return;
    actionHeld.current = true;
    setBusy(true);
    const gen = ++actionGen.current;
    void (async () => {
      try {
        await work();
      } catch (error) {
        if (gen !== actionGen.current) return;
        reportError(rpcError('process action failed', error));
      } finally {
        if (gen !== actionGen.current) return;
        actionHeld.current = false;
        setBusy(false);
      }
    })();
  }, [reportError]);

  const spawnProcess = useCallback(() => {
    const trimmed = commandDraft.trim();
    const cwd = (spawnCwd ?? '').trim();
    if (!trimmed) return;
    if (!cwd) {
      reportError('Session working directory unknown — cannot spawn.');
      return;
    }
    const specLabel = processSpawnLabel(labelDraft);
    if (specLabel === null) {
      reportError('Label must be 1 to 64 bytes without NUL');
      return;
    }
    runAction(async () => {
      const response = await props.client.command({
        type: 'process_spawn',
        spec: {
          argv: ['bash', '-c', trimmed],
          cwd,
          tty: true,
          terminalSize: { rows: DEFAULT_ROWS, cols: DEFAULT_COLS },
          ...(specLabel !== '' ? { label: specLabel } : {}),
        },
      }, props.sessionId);
      if (!response.success) {
        reportError(rpcError('process_spawn failed', response.error));
        return;
      }
      const spawned = parseProcessInfo(response.data);
      setCommandDraft('');
      setLabelDraft('');
      await loadList();
      if (spawned !== null) openProcess(spawned.id);
    });
  }, [
    commandDraft,
    labelDraft,
    loadList,
    openProcess,
    props.client,
    props.sessionId,
    reportError,
    runAction,
    spawnCwd,
  ]);

  const sendInput = useCallback(() => {
    const processId = selectedId;
    const data = inputDraft;
    if (processId === null || data === '') return;
    runAction(async () => {
      const response = await props.client.command({
        type: 'process_write',
        processId,
        dataBase64: encodeTmuxTerminalData(data),
      }, props.sessionId);
      if (!response.success) {
        reportError(rpcError('process_write failed', response.error));
        return;
      }
      setInputDraft('');
      await loadLogs(processId, logCursor, true);
    });
  }, [inputDraft, loadLogs, logCursor, props.client, props.sessionId, reportError, runAction, selectedId]);

  const sendKey = useCallback((key: ProcessKey) => {
    const processId = selectedId;
    if (processId === null) return;
    runAction(async () => {
      const response = await props.client.command({
        type: 'process_keys',
        processId,
        keys: [key],
      }, props.sessionId);
      if (!response.success) {
        reportError(rpcError('process_keys failed', response.error));
        return;
      }
      await loadLogs(processId, logCursor, true);
    });
  }, [loadLogs, logCursor, props.client, props.sessionId, reportError, runAction, selectedId]);

  const resizeProcess = useCallback(() => {
    const processId = selectedId;
    if (processId === null) return;
    const cols = parseBoundedInt(colsDraft, DEFAULT_COLS, 65535);
    const rows = parseBoundedInt(rowsDraft, DEFAULT_ROWS, 65535);
    if (cols === null || rows === null) {
      reportError('Resize needs integer cols and rows from 1 to 65535');
      return;
    }
    runAction(async () => {
      const response = await props.client.command({
        type: 'process_resize',
        processId,
        cols,
        rows,
      }, props.sessionId);
      if (!response.success) {
        reportError(rpcError('process_resize failed', response.error));
      }
    });
  }, [colsDraft, props.client, props.sessionId, reportError, rowsDraft, runAction, selectedId]);

  const signalProcess = useCallback((signal: ProcessSignal) => {
    const processId = selectedId;
    if (processId === null) return;
    runAction(async () => {
      const response = await props.client.command({
        type: 'process_signal',
        processId,
        signal,
      }, props.sessionId);
      if (!response.success) {
        reportError(rpcError('process_signal failed', response.error));
        return;
      }
      await loadList();
      await loadDetail(processId);
    });
  }, [loadDetail, loadList, props.client, props.sessionId, reportError, runAction, selectedId]);

  const stopProcess = useCallback(() => {
    const processId = selectedId;
    if (processId === null) return;
    runAction(async () => {
      const response = await props.client.command({ type: 'process_stop', processId }, props.sessionId);
      if (!response.success) {
        reportError(rpcError('process_stop failed', response.error));
        return;
      }
      const stopped = parseProcessInfo(response.data);
      if (stopped !== null) setDetail(stopped);
      await loadList();
      await loadLogs(processId, logCursor, true);
    });
  }, [loadList, loadLogs, logCursor, props.client, props.sessionId, reportError, runAction, selectedId]);

  const waitProcess = useCallback(() => {
    const processId = selectedId;
    if (processId === null) return;
    const timeoutMs = parseBoundedInt(waitDraft, WAIT_TIMEOUT_MS, 600000);
    if (timeoutMs === null) {
      reportError('Wait timeout must be an integer from 1 to 600000');
      return;
    }
    runAction(async () => {
      const response = await props.client.command({
        type: 'process_wait',
        processId,
        timeoutMs,
      }, props.sessionId);
      if (!response.success) {
        reportError(rpcError('process_wait failed', response.error));
        return;
      }
      const waited = parseProcessInfo(response.data);
      if (waited !== null) setDetail(waited);
      await loadList();
    });
  }, [loadList, props.client, props.sessionId, reportError, runAction, selectedId, waitDraft]);

  useEffect(() => {
    listGen.current += 1;
    logsGen.current += 1;
    detailGen.current += 1;
    cwdGen.current += 1;
    actionGen.current += 1;
    actionHeld.current = false;
    selectedIdRef.current = null;
    logCursorRef.current = 0;
    setProcesses(null);
    setSelectedId(null);
    setDetail(null);
    setLogText('');
    setLogCursor(0);
    setLogEof(false);
    setSpawnCwd(null);
    setBusy(false);
    void loadCwd();
    void loadList();
    const poll = setInterval(() => {
      if (actionHeld.current) return;
      void loadList();
      const openId = selectedIdRef.current;
      if (openId !== null) void loadLogs(openId, logCursorRef.current, true);
    }, POLL_MS);
    const off = props.client.subscribeFrames((frame) => {
      if (frameSessionId(frame) !== props.sessionId) return;
      if (!frame || typeof frame !== 'object') return;
      const record = frame as Record<string, unknown>;
      const kind = record.type;
      if (kind === 'process_started' || kind === 'process_exited') {
        void loadList();
        const row = parseProcessInfo(record.process);
        if (row !== null && selectedIdRef.current === row.id) setDetail(row);
        return;
      }
      if (kind !== 'process_output') return;
      const processId = typeof record.id === 'string' ? record.id : null;
      if (processId === null || processId !== selectedIdRef.current) return;
      if (typeof record.dataBase64 !== 'string') return;
      const bytes = decodeTmuxTerminalData(record.dataBase64);
      if (bytes === null) return;
      const chunk = redactSecrets(Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength).toString('utf8'));
      const nextCursor = typeof record.cursor === 'number' && Number.isInteger(record.cursor)
        ? record.cursor
        : null;
      setLogText((current) => clipLogText(current + chunk));
      if (nextCursor !== null) setLogCursor(nextCursor);
    });
    return () => {
      listGen.current += 1;
      logsGen.current += 1;
      detailGen.current += 1;
      cwdGen.current += 1;
      actionGen.current += 1;
      clearInterval(poll);
      off();
    };
  }, [loadCwd, loadList, loadLogs, props.client, props.sessionId]);

  const selected = detail ?? processes?.find((row) => row.id === selectedId) ?? null;
  const live = selected !== null && isLiveProcess(selected.state);
  const canSpawn = !busy && commandDraft.trim() !== '' && (spawnCwd ?? '').trim() !== '';

  const closeOrBack = useCallback(() => {
    if (selectedId !== null) {
      logsGen.current += 1;
      detailGen.current += 1;
      selectedIdRef.current = null;
      setSelectedId(null);
      setDetail(null);
      setLogText('');
      setLogCursor(0);
      setLogEof(false);
      return;
    }
    props.onClose();
  }, [selectedId, props.onClose]);

  const registerBackHandler = props.onRegisterBackHandler;
  useEffect(() => {
    registerBackHandler?.(() => {
      closeOrBack();
      return true;
    });
    return () => registerBackHandler?.(null);
  }, [registerBackHandler, closeOrBack]);

  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar
        palette={palette}
        title="Processes"
        onBack={closeOrBack}
        actions={[{ icon: 'refresh', label: 'Refresh', onPress: () => { void loadCwd(); void loadList(); }, disabled: busy }]}
      />
      <ScrollView style={styles.list} keyboardShouldPersistTaps="handled">
        {selectedId === null && (
          <SpawnForm
            palette={palette}
            commandDraft={commandDraft}
            labelDraft={labelDraft}
            busy={busy}
            canSpawn={canSpawn}
            onCommand={setCommandDraft}
            onLabel={setLabelDraft}
            onSpawn={spawnProcess}
          />
        )}
        {processes === null && (
          <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]}>--</Text>
        )}
        {processes !== null && processes.length === 0 && selectedId === null && (
          <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]}>No processes</Text>
        )}
        {selectedId === null && processes !== null && processes.map((row) => (
          <ProcessRow key={row.id} palette={palette} row={row} onOpen={openProcess} />
        ))}
        {selected !== null && (
          <ProcessDetailPane
            palette={palette}
            selected={selected}
            logText={logText}
            logEof={logEof}
            live={live}
            busy={busy}
            inputDraft={inputDraft}
            colsDraft={colsDraft}
            rowsDraft={rowsDraft}
            waitDraft={waitDraft}
            onLoadLogs={() => { if (selectedId !== null) void loadLogs(selectedId, logCursor, true); }}
            onInput={setInputDraft}
            onSend={sendInput}
            onKey={sendKey}
            onCols={setColsDraft}
            onRows={setRowsDraft}
            onResize={resizeProcess}
            onStop={stopProcess}
            onSignal={signalProcess}
            onWaitDraft={setWaitDraft}
            onWait={waitProcess}
          />
        )}
        <View style={styles.bottomPad} />
      </ScrollView>
    </View>
  );
}
