// Exact 20 MiB native v3 JSONL session generator/loader for the P0 resource
// acceptance harness. Injectable writer/fs so unit tests never need rpi.
//
// File shape mirrors `pi_coding::start_session_in` / projects.sh seeds:
//   {"type":"session","version":3,...}
//   {"type":"session_info",...}
//   {"type":"message",...} * N   (off-branch 1 MiB padding siblings)
//   {"type":"message",...}       (compact selected-leaf marker; LAST)
// Every JSONL line stays under the 8 MiB native line cap. The on-disk size
// is EXACTLY 20 MiB (20971520 bytes), including newlines. Padding is never
// reconstructed into `load_session_tree(...).build_context(None).messages`.

import { createWriteStream } from 'node:fs';
import { open as defaultOpen, stat as defaultStat, mkdir as defaultMkdir } from 'node:fs/promises';
import { dirname } from 'node:path';

export const EXACT_NATIVE_SESSION_BYTES = 20 * 1024 * 1024;
export const MAX_NATIVE_LINE_BYTES = 8 * 1024 * 1024;
export const NATIVE_SESSION_VERSION = 3;
export const DEFAULT_NATIVE_SESSION_ID = 'p0-native-20mib';
export const DEFAULT_NATIVE_MARKER = 'p0-native-20mib-marker';
export const EXACT_SELECTED_PRESSURE_BYTES = 2 * 1024 * 1024;
export const MIN_SELECTED_CONTEXT_BYTES = 1024 * 1024;
export const MAX_SELECTED_PRESSURE_CONTEXT_BYTES = 4 * 1024 * 1024;
export const DEFAULT_PRESSURE_SESSION_ID = 'p0-pressure-2mib';
export const DEFAULT_PRESSURE_MARKER = 'p0-pressure-2mib-marker';

const FILLER_LINE_BYTES = 1024 * 1024;
export const MAX_SELECTED_CONTEXT_BYTES = 64 * 1024;
export const NATIVE_MARKER_ENTRY_ID = 'm-marker';

export function encodeCwdDir(cwd) {
  let encoded = String(cwd);
  if (encoded.startsWith('/') || encoded.startsWith('\\')) encoded = encoded.slice(1);
  return `--${encoded.replace(/[/\\:]/g, '-')}--`;
}

export function nativeSessionFileName(sessionId, timestamp) {
  const day = String(timestamp).slice(0, 10);
  return `${day}T000000-${sessionId}.jsonl`;
}

export function nativeSessionPath({ sessionsRoot, cwd, sessionId, timestamp }) {
  return `${sessionsRoot.replace(/[/\\]$/, '')}/${encodeCwdDir(cwd)}/${nativeSessionFileName(sessionId, timestamp)}`;
}

function headerRecord(sessionId, timestamp, cwd) {
  return { type: 'session', version: NATIVE_SESSION_VERSION, id: sessionId, timestamp, cwd };
}

function sessionInfoRecord(timestamp, name) {
  return { type: 'session_info', id: 'si-1', parentId: null, timestamp, name };
}

function messageRecord(id, timestamp, text, tsNum, parentId = null) {
  return {
    type: 'message',
    id,
    parentId,
    timestamp,
    message: {
      role: 'user',
      content: [{ type: 'text', text }],
      timestamp: tsNum,
    },
  };
}

export function jsonlLine(record) {
  return `${JSON.stringify(record)}\n`;
}

function lineBytes(record) {
  return Buffer.byteLength(jsonlLine(record), 'utf8');
}

function paddedMessageLine(id, timestamp, tsNum, targetBytes, textPrefix = '', parentId = null) {
  const empty = messageRecord(id, timestamp, textPrefix, tsNum, parentId);
  const emptyBytes = lineBytes(empty);
  const pad = targetBytes - emptyBytes;
  if (pad < 0) {
    throw new Error(`message ${id} overhead ${emptyBytes} exceeds target line ${targetBytes}`);
  }
  const line = jsonlLine(messageRecord(id, timestamp, `${textPrefix}${'x'.repeat(pad)}`, tsNum, parentId));
  if (Buffer.byteLength(line, 'utf8') !== targetBytes) {
    throw new Error(`message ${id} padded to ${Buffer.byteLength(line, 'utf8')}, expected ${targetBytes}`);
  }
  if (targetBytes > MAX_NATIVE_LINE_BYTES) {
    throw new Error(`message ${id} line ${targetBytes} exceeds ${MAX_NATIVE_LINE_BYTES}`);
  }
  return line;
}

const SKIP_LEAF_TYPES = new Set(['session', 'label', 'checkpoint']);

export function parseNativeSessionRecords(text) {
  if (typeof text !== 'string' || text === '') {
    throw new Error('parseNativeSessionRecords: text is required');
  }
  const lines = text.endsWith('\n') ? text.slice(0, -1).split('\n') : text.split('\n');
  const records = [];
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    const bytes = Buffer.byteLength(line, 'utf8');
    if (bytes > MAX_NATIVE_LINE_BYTES) {
      throw new Error(`parseNativeSessionRecords: line ${i + 1} is ${bytes} bytes, exceeds ${MAX_NATIVE_LINE_BYTES}`);
    }
    if (line === '') continue;
    try {
      records.push(JSON.parse(line));
    } catch {
      throw new Error(`parseNativeSessionRecords: line ${i + 1} is not JSON`);
    }
  }
  return records;
}

export function nativeSelectedLeafId(records) {
  if (!Array.isArray(records)) return null;
  for (let i = records.length - 1; i >= 0; i -= 1) {
    const rec = records[i];
    if (!rec || typeof rec !== 'object') continue;
    if (SKIP_LEAF_TYPES.has(rec.type)) continue;
    if (typeof rec.id === 'string' && rec.id !== '') return rec.id;
  }
  return null;
}

export function nativeSelectedBranch(records) {
  if (!Array.isArray(records)) return [];
  const byId = new Map();
  for (const rec of records) {
    if (rec && rec.type !== 'session' && typeof rec.id === 'string' && rec.id !== '') {
      byId.set(rec.id, rec);
    }
  }
  const leafId = nativeSelectedLeafId(records);
  if (!leafId || !byId.has(leafId)) return [];
  const branch = [];
  const visited = new Set();
  let current = leafId;
  while (current && byId.has(current) && !visited.has(current)) {
    visited.add(current);
    const rec = byId.get(current);
    branch.push(rec);
    current = typeof rec.parentId === 'string' && rec.parentId !== '' ? rec.parentId : null;
  }
  branch.reverse();
  return branch;
}

export function nativeBranchContextMessages(branch) {
  if (!Array.isArray(branch)) return [];
  return branch.filter((rec) => rec && rec.type === 'message');
}
export function nativeMessageUserText(record) {
  const content = record && record.message && record.message.content;
  if (!Array.isArray(content)) return '';
  return content
    .map((block) => (block && block.type === 'text' && typeof block.text === 'string' ? block.text : ''))
    .join('');
}


export function inspectNativeParentChain(records, marker) {
  if (!Array.isArray(records) || records.length < 2) {
    throw new Error('inspectNativeParentChain: session records missing');
  }
  const header = records[0];
  if (!header || header.type !== 'session' || header.version !== NATIVE_SESSION_VERSION) {
    throw new Error(`inspectNativeParentChain: not a v3 native session header: ${JSON.stringify(header)}`);
  }
  const leafId = nativeSelectedLeafId(records);
  if (!leafId) throw new Error('inspectNativeParentChain: selected leaf missing');
  const branch = nativeSelectedBranch(records);
  if (!branch.length || branch[branch.length - 1].id !== leafId) {
    throw new Error('inspectNativeParentChain: selected branch does not end at the active leaf');
  }
  const messages = nativeBranchContextMessages(branch);
  if (!messages.length) {
    throw new Error('inspectNativeParentChain: selected branch has no messages');
  }
  const leaf = branch[branch.length - 1];
  const leafText = nativeMessageUserText(leaf);
  if (marker && leafText !== marker) {
    throw new Error(`inspectNativeParentChain: selected leaf ${leaf.id} user text does not equal marker`);
  }
  const contextPayload = messages.map((rec) => rec.message);
  const selectedContextBytes = Buffer.byteLength(JSON.stringify(contextPayload), 'utf8');
  if (selectedContextBytes > MAX_SELECTED_CONTEXT_BYTES) {
    throw new Error(`inspectNativeParentChain: selected-branch context ${selectedContextBytes} exceeds ${MAX_SELECTED_CONTEXT_BYTES}`);
  }
  return {
    header,
    selectedLeafId: leafId,
    selectedBranchIds: branch.map((rec) => rec.id),
    selectedMessageCount: messages.length,
    selectedContextBytes,
    selectedUserText: leafText,
    markerOnSelectedBranch: !marker || leafText === marker,
  };
}

function defaultCreateWriter(filePath) {
  const stream = createWriteStream(filePath);
  let bytes = 0;
  return {
    write(chunk) {
      const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk, 'utf8');
      bytes += buf.length;
      return new Promise((resolve, reject) => {
        stream.write(buf, (err) => (err ? reject(err) : resolve()));
      });
    },
    close() {
      return new Promise((resolve, reject) => {
        stream.end((err) => (err ? reject(err) : resolve()));
      });
    },
    bytesWritten() {
      return bytes;
    },
  };
}

export async function writeExactNativeSession({
  filePath,
  cwd,
  sessionId = DEFAULT_NATIVE_SESSION_ID,
  timestamp = '2026-08-31T00:00:00.000Z',
  marker = DEFAULT_NATIVE_MARKER,
  targetBytes = EXACT_NATIVE_SESSION_BYTES,
  name = 'p0-native-20mib',
  mkdir = defaultMkdir,
  createWriter = defaultCreateWriter,
} = {}) {
  if (!filePath) throw new Error('writeExactNativeSession: filePath is required');
  if (!cwd) throw new Error('writeExactNativeSession: cwd is required');
  if (!Number.isInteger(targetBytes) || targetBytes < 1024) {
    throw new Error(`writeExactNativeSession: invalid targetBytes ${targetBytes}`);
  }

  await mkdir(dirname(filePath), { recursive: true });
  const header = jsonlLine(headerRecord(sessionId, timestamp, cwd));
  const info = jsonlLine(sessionInfoRecord(timestamp, name));
  const prefixBytes = Buffer.byteLength(header, 'utf8') + Buffer.byteLength(info, 'utf8');
  if (prefixBytes >= targetBytes) {
    throw new Error(`writeExactNativeSession: prefix ${prefixBytes} >= target ${targetBytes}`);
  }

  const writer = createWriter(filePath);
  const markerEntryId = NATIVE_MARKER_ENTRY_ID;
  const markerLine = jsonlLine(messageRecord(markerEntryId, timestamp, marker, 1, 'si-1'));
  const markerBytes = Buffer.byteLength(markerLine, 'utf8');
  if (prefixBytes + markerBytes > targetBytes) {
    throw new Error(`writeExactNativeSession: prefix+marker ${prefixBytes + markerBytes} > target ${targetBytes}`);
  }
  let lastFillerId = null;
  try {
    await writer.write(header);
    await writer.write(info);

    let remaining = targetBytes - prefixBytes - markerBytes;
    let index = 0;
    const minLine = lineBytes(messageRecord('m999', timestamp, '', 0, 'si-1'));
    while (remaining > 0) {
      let chunk = Math.min(FILLER_LINE_BYTES, remaining);
      if (remaining - chunk > 0 && remaining - chunk < minLine) {
        chunk = remaining;
      }
      if (chunk > MAX_NATIVE_LINE_BYTES) chunk = MAX_NATIVE_LINE_BYTES;
      if (chunk < minLine) {
        throw new Error(`writeExactNativeSession: remaining ${remaining} cannot form a valid message line`);
      }
      const id = index === 0 ? 'm-off' : `m${index}`;
      const parentId = index === 0 ? 'si-1' : (index === 1 ? 'm-off' : `m${index - 1}`);
      const line = paddedMessageLine(id, timestamp, index, chunk, '', parentId);
      await writer.write(line);
      remaining -= chunk;
      lastFillerId = id;
      index += 1;
    }
    await writer.write(markerLine);
  } finally {
    await writer.close();
  }

  const written = writer.bytesWritten();
  if (written !== targetBytes) {
    throw new Error(`writeExactNativeSession: wrote ${written} bytes, expected ${targetBytes}`);
  }
  return {
    path: filePath,
    bytes: written,
    sessionId,
    cwd,
    timestamp,
    marker,
    recordCountHint: null,
    selectedLeafId: markerEntryId,
    markerEntryId,
    offBranchId: lastFillerId ? 'm-off' : null,
  };
}

// Exact 2 MiB selected-branch native v3 JSONL session generator for the P0
// connection-pressure harness. Same file shape as writeExactNativeSession
// (v3 header + session_info + chained user messages + compact marker leaf
// LAST), but EVERY message lives on the selected branch: the parentId chain
// runs si-1 -> m0000 -> m0001 -> ... -> m-marker with no off-branch padding.
// The on-disk size is EXACTLY 2 MiB (2097152 bytes) including newlines, every
// line stays under the 8 MiB native cap, and the selected-branch context
// (JSON of the branch message payloads) is >= 1 MiB and <= 4 MiB.
export async function writeExactSelectedPressureSession({
  filePath,
  cwd,
  sessionId = DEFAULT_PRESSURE_SESSION_ID,
  timestamp = '2026-08-31T00:00:00.000Z',
  marker = DEFAULT_PRESSURE_MARKER,
  targetBytes = EXACT_SELECTED_PRESSURE_BYTES,
  name = 'p0-pressure-2mib',
  mkdir = defaultMkdir,
  createWriter = defaultCreateWriter,
} = {}) {
  if (!filePath) throw new Error('writeExactSelectedPressureSession: filePath is required');
  if (!cwd) throw new Error('writeExactSelectedPressureSession: cwd is required');
  if (!Number.isInteger(targetBytes) || targetBytes < 1024) {
    throw new Error(`writeExactSelectedPressureSession: invalid targetBytes ${targetBytes}`);
  }

  await mkdir(dirname(filePath), { recursive: true });
  const header = jsonlLine(headerRecord(sessionId, timestamp, cwd));
  const info = jsonlLine(sessionInfoRecord(timestamp, name));
  const prefixBytes = Buffer.byteLength(header, 'utf8') + Buffer.byteLength(info, 'utf8');
  if (prefixBytes >= targetBytes) {
    throw new Error(`writeExactSelectedPressureSession: prefix ${prefixBytes} >= target ${targetBytes}`);
  }

  const markerEntryId = NATIVE_MARKER_ENTRY_ID;
  // Filler ids are fixed-width (`m0000`..`m9999`), so the marker's parentId
  // is always 5 bytes and the marker line size is known before fillers exist.
  const markerProbe = jsonlLine(messageRecord(markerEntryId, timestamp, marker, 1, 'm0000'));
  const markerBytes = Buffer.byteLength(markerProbe, 'utf8');
  if (prefixBytes + markerBytes > targetBytes) {
    throw new Error(`writeExactSelectedPressureSession: prefix+marker ${prefixBytes + markerBytes} > target ${targetBytes}`);
  }

  const writer = createWriter(filePath);
  const lines = [];
  let lastFillerId = null;
  try {
    await writer.write(header);
    await writer.write(info);
    lines.push(header, info);

    let remaining = targetBytes - prefixBytes - markerBytes;
    let index = 0;
    const minLine = lineBytes(messageRecord('m9999', timestamp, '', 0, 'm0000'));
    while (remaining > 0) {
      let chunk = Math.min(FILLER_LINE_BYTES, remaining);
      if (remaining - chunk > 0 && remaining - chunk < minLine) {
        chunk = remaining;
      }
      if (chunk > MAX_NATIVE_LINE_BYTES) chunk = MAX_NATIVE_LINE_BYTES;
      if (chunk < minLine) {
        throw new Error(`writeExactSelectedPressureSession: remaining ${remaining} cannot form a valid message line`);
      }
      const id = `m${String(index).padStart(4, '0')}`;
      const parentId = index === 0 ? 'si-1' : `m${String(index - 1).padStart(4, '0')}`;
      const line = paddedMessageLine(id, timestamp, index, chunk, '', parentId);
      await writer.write(line);
      lines.push(line);
      remaining -= chunk;
      lastFillerId = id;
      index += 1;
    }

    const markerLine = jsonlLine(messageRecord(markerEntryId, timestamp, marker, 1, lastFillerId));
    if (Buffer.byteLength(markerLine, 'utf8') !== markerBytes) {
      throw new Error(`writeExactSelectedPressureSession: marker line ${Buffer.byteLength(markerLine, 'utf8')} !== ${markerBytes}`);
    }
    await writer.write(markerLine);
    lines.push(markerLine);
  } finally {
    await writer.close();
  }

  const written = writer.bytesWritten();
  if (written !== targetBytes) {
    throw new Error(`writeExactSelectedPressureSession: wrote ${written} bytes, expected ${targetBytes}`);
  }

  const records = parseNativeSessionRecords(lines.join(''));
  const branch = nativeSelectedBranch(records);
  const messages = nativeBranchContextMessages(branch);
  const leaf = records[records.length - 1];
  if (leaf.type !== 'message' || leaf.id !== markerEntryId || nativeMessageUserText(leaf) !== marker) {
    throw new Error('writeExactSelectedPressureSession: marker is not on the exact leaf');
  }
  if (messages.length !== records.length - 2) {
    throw new Error(`writeExactSelectedPressureSession: ${messages.length} branch messages, expected ${records.length - 2} (off-branch padding present)`);
  }
  const contextPayload = messages.map((rec) => rec.message);
  const selectedContextBytes = Buffer.byteLength(JSON.stringify(contextPayload), 'utf8');
  if (selectedContextBytes < MIN_SELECTED_CONTEXT_BYTES || selectedContextBytes > MAX_SELECTED_PRESSURE_CONTEXT_BYTES) {
    throw new Error(`writeExactSelectedPressureSession: selected context ${selectedContextBytes} outside [${MIN_SELECTED_CONTEXT_BYTES}, ${MAX_SELECTED_PRESSURE_CONTEXT_BYTES}]`);
  }

  return {
    path: filePath,
    bytes: written,
    sessionId,
    cwd,
    timestamp,
    marker,
    selectedContextBytes,
    leaf: markerEntryId,
    selectedLeafId: markerEntryId,
    selectedMessageCount: messages.length,
  };
}

export async function loadNativeSession(filePath, {
  expectedBytes = EXACT_NATIVE_SESSION_BYTES,
  expectedId = null,
  expectedCwd = null,
  marker = null,
  fsp = { open: defaultOpen, stat: defaultStat },
} = {}) {
  if (!filePath) throw new Error('loadNativeSession: filePath is required');
  let st;
  try {
    st = await fsp.stat(filePath);
  } catch (err) {
    throw new Error(`loadNativeSession: missing session file ${filePath}: ${err && err.message ? err.message : err}`);
  }
  if (!Number.isFinite(st.size)) {
    throw new Error(`loadNativeSession: size unreadable for ${filePath}`);
  }
  if (st.size !== expectedBytes) {
    throw new Error(`loadNativeSession: size ${st.size} !== ${expectedBytes}`);
  }

  const fh = await fsp.open(filePath, 'r');
  try {
    const buf = Buffer.alloc(st.size);
    let offset = 0;
    while (offset < st.size) {
      const { bytesRead } = await fh.read(buf, offset, st.size - offset, offset);
      if (!bytesRead) break;
      offset += bytesRead;
    }
    if (offset !== st.size) {
      throw new Error(`loadNativeSession: short read ${offset} !== ${st.size}`);
    }
    if (buf[st.size - 1] !== 0x0a) {
      throw new Error('loadNativeSession: file does not end with a JSONL newline');
    }
    const text = buf.toString('utf8');
    const records = parseNativeSessionRecords(text);
    const chain = inspectNativeParentChain(records, marker);
    const header = chain.header;
    if (!header.id || typeof header.id !== 'string') {
      throw new Error('loadNativeSession: header id missing');
    }
    if (expectedId && header.id !== expectedId) {
      throw new Error(`loadNativeSession: header id ${header.id} !== ${expectedId}`);
    }
    if (expectedCwd && header.cwd !== expectedCwd) {
      throw new Error(`loadNativeSession: header cwd ${header.cwd} !== ${expectedCwd}`);
    }

    return {
      path: filePath,
      bytes: st.size,
      header,
      sessionId: header.id,
      cwd: header.cwd,
      timestamp: header.timestamp,
      selectedLeafId: chain.selectedLeafId,
      selectedBranchIds: chain.selectedBranchIds,
      selectedMessageCount: chain.selectedMessageCount,
      selectedContextBytes: chain.selectedContextBytes,
      selectedUserText: chain.selectedUserText,
      markerOnSelectedBranch: chain.markerOnSelectedBranch,
    };
  } finally {
    await fh.close();
  }
}
