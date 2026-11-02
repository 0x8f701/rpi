// File conversation — the Explorer's per-file Ask mode.
//
// FileExplorerPanel binds ONE conversation (a client-minted UUID) to one open
// file path and owns every wire command plus the snapshot state. This view is
// the integrated phone surface for that binding: the loaded source excerpt
// with exact line selection, the context the engine reports (requested /
// included range, truncation), the transcript mapped through the same
// MarkdownBody + TranscriptItemView pairing the side chat uses, and one
// composer whose Send / Stop target exactly the bound conversation.
//
// Nothing is parsed here: payloads arrive already normalized fail-closed in
// ../protocol (parseFileConversationSnapshot, buildFileConversationPromptPayload),
// so this file adds no second wire reader. Every failure — refused payloads,
// busy verdicts, prompt-gate refusals — surfaces through the panel's central
// onError; this surface renders no inline RPC error of its own.

import React, { useEffect, useRef } from 'react';
import {
  KeyboardAvoidingView,
  Platform,
  ScrollView,
  Text,
  TextInput,
  TouchableOpacity,
  View,
  type NativeSyntheticEvent,
  type TextInputKeyPressEventData,
} from 'react-native';
import { MarkdownBody } from '../MarkdownBody';
import { redactSecrets } from '../redact';
import type { FileExplorerBlameResult, FileExplorerReadResult } from '../fileExplorer';
import { TranscriptItemView } from '../TranscriptItemView';
import type {
  FileConversationContextWire,
  FileConversationRangeWire,
  FileConversationSnapshotData,
  SideChatEntryWire,
} from '../protocol';
import { styles, type Palette } from '../ui';

/** Bounded height of the source excerpt above the transcript: the file pane
 *  remains the place that pages the whole file; Ask shows what it loaded. */
const SOURCE_EXCERPT_MAX_HEIGHT = 220;

/** One-line summary of the file context the engine attached to the last
 *  snapshot: what was requested, what was included, whether it was truncated.
 *  Null when the engine sent no context block. */
export function fileConversationContextLine(
  context: FileConversationContextWire | undefined,
): string | null {
  if (!context) return null;
  const requested = context.requestedRange
    ? ` of requested ${context.requestedRange.startLine}–${context.requestedRange.endLine}`
    : '';
  const included = context.includedRange
    ? `Included lines ${context.includedRange.startLine}–${context.includedRange.endLine}`
    : 'No file lines included';
  return `${included}${requested}${context.truncated ? ' · truncated' : ''}`;
}

/** Mirrors SideChatScreen's private toolArgsText: string args pass through,
 *  everything else renders as JSON (or nothing when it cannot serialize). */
function toolArgsText(args: unknown): string {
  if (args == null) return '';
  if (typeof args === 'string') return args;
  try {
    return JSON.stringify(args);
  } catch {
    return '';
  }
}

/** Transcript row mapping — the same shape SideChatScreen renders (tool cards
 *  and thinking folds via TranscriptItemView, prose via MarkdownBody), so a
 *  file-conversation reply looks exactly like the chat surfaces around it. */
function FileConversationEntryRow(props: { entry: SideChatEntryWire; palette: Palette }): React.JSX.Element {
  const { entry, palette } = props;
  if (entry.kind === 'tool') {
    return (
      <TranscriptItemView
        item={{
          kind: 'tool',
          id: entry.id,
          toolCallId: entry.id,
          toolName: entry.toolName ?? '',
          args: toolArgsText(entry.toolArgs),
          result: entry.output ?? '',
          status: entry.isPartial ? 'running' : entry.isError ? 'error' : 'done',
        }}
        palette={palette}
      />
    );
  }
  if (entry.kind === 'thinking') {
    return (
      <TranscriptItemView
        item={{
          kind: 'assistant',
          id: entry.id,
          status: entry.isPartial ? 'streaming' : 'final',
          blocks: [{ type: 'thinking', thinking: entry.thinking || entry.text }],
        }}
        palette={palette}
      />
    );
  }
  return (
    <View>
      {entry.kind === 'assistant' && entry.thinking ? (
        <TranscriptItemView
          item={{
            kind: 'assistant',
            id: `${entry.id}-thinking`,
            status: 'final',
            blocks: [{ type: 'thinking', thinking: entry.thinking }],
          }}
          palette={palette}
        />
      ) : null}
      <MarkdownBody text={entry.text} palette={palette} />
    </View>
  );
}

/** The loaded source, one tappable row per line. Line numbers are exact, not
 *  guessed: the explorer reader's cursor is the 0-based line index where the
 *  next page starts (crates/pi-cli/src/file_explorer.rs `next_cursor`), the
 *  first read never sends a cursor (starts at line 1), and pages append
 *  contiguously — so row `index + 1` IS the file line a selection names. */
function SourceExcerpt(props: {
  preview: FileExplorerReadResult;
  palette: Palette;
  selection: FileConversationRangeWire | null;
  onSelectLine: (line: number) => void;
}): React.JSX.Element {
  const { preview, palette, selection } = props;
  return (
    <View>
      <ScrollView
        style={{ maxHeight: SOURCE_EXCERPT_MAX_HEIGHT }}
        nestedScrollEnabled
        keyboardShouldPersistTaps="handled">
        {preview.lines.map((line, index) => {
          const lineNumber = index + 1;
          const selected = selection != null
            && lineNumber >= selection.startLine
            && lineNumber <= selection.endLine;
          return (
            <TouchableOpacity
              key={lineNumber}
              onPress={() => props.onSelectLine(lineNumber)}
              style={{
                flexDirection: 'row',
                backgroundColor: selected ? palette.accent : palette.bg,
                minHeight: 22,
              }}
              accessibilityRole="button"
              accessibilityLabel={`Line ${lineNumber}`}
              accessibilityState={{ selected }}>
              <Text
                style={{
                  width: 40,
                  textAlign: 'right',
                  paddingRight: 8,
                  color: selected ? palette.bg : palette.muted,
                  fontFamily: palette.mono,
                  fontSize: 11,
                  paddingVertical: 3,
                }}>
                {lineNumber}
              </Text>
              <Text
                style={{
                  flex: 1,
                  color: selected ? palette.bg : palette.fg,
                  fontFamily: palette.mono,
                  fontSize: 12,
                  paddingVertical: 3,
                }}
                numberOfLines={1}>
                {line.length === 0 ? ' ' : redactSecrets(line)}
              </Text>
            </TouchableOpacity>
          );
        })}
      </ScrollView>
      <Text style={[styles.cardMeta, { color: palette.muted }]}>
        {preview.lines.length} / {preview.totalLines}
      </Text>
    </View>
  );
}

/** Compact one-line attribution. A null commit is Uncommitted, never a zero oid. */
export function blameAttributionLabel(line: FileExplorerBlameResult['lines'][number]): string {
  const who = line.commit == null ? 'Uncommitted' : line.author;
  const when = new Date(line.time * 1000).toISOString().slice(0, 10);
  const short = line.commit == null ? '' : ` ${line.commit.slice(0, 7)}`;
  return `${line.currentLine} ${who}${short} ${when}`;
}

export function FileConversationView(props: {
  palette: Palette;
  /** Bound resolved preview path — the exact path this conversation opened. */
  path: string;
  preview: FileExplorerReadResult | null;
  snapshot: FileConversationSnapshotData | null;
  /** Last snapshot payload was refused (malformed or foreign path). */
  unreadable: boolean;
  draft: string;
  busy: boolean;
  selection: FileConversationRangeWire | null;
  onDraftChange: (text: string) => void;
  onSelectionChange: (selection: FileConversationRangeWire | null) => void;
  onSend: () => void;
  onStop: () => void;
  /** Bumped by Ask-about-selection; focuses the composer without touching the draft. */
  focusComposer?: number;
  blame?: FileExplorerBlameResult | null;
  blamePending?: boolean;
  onBlame?: () => void;
}): React.JSX.Element {
  const { palette, snapshot, preview, selection } = props;
  const composerRef = useRef<TextInput>(null);
  const focusRequest = props.focusComposer ?? 0;
  useEffect(() => {
    if (focusRequest === 0) return;
    const handle = requestAnimationFrame(() => composerRef.current?.focus());
    return () => cancelAnimationFrame(handle);
  }, [focusRequest]);
  const streaming = snapshot?.streaming === true;
  const entries = snapshot?.entries ?? [];
  const contextLine = fileConversationContextLine(snapshot?.context);

  const onSelectLine = (line: number) => {
    const current = selection;
    // Tapping inside the selection clears it; otherwise anchor or extend.
    if (current && line >= current.startLine && line <= current.endLine) {
      props.onSelectionChange(null);
      return;
    }
    if (!current) {
      props.onSelectionChange({ startLine: line, endLine: line });
      return;
    }
    props.onSelectionChange({
      startLine: Math.min(current.startLine, line),
      endLine: Math.max(current.endLine, line),
    });
  };

  const onComposerKey = (event: NativeSyntheticEvent<TextInputKeyPressEventData>) => {
    const native = event.nativeEvent as { key?: string; shiftKey?: boolean };
    if (native.key !== 'Enter' || native.shiftKey === true) return;
    if (streaming || props.busy) return;
    props.onSend();
  };

  return (
    <KeyboardAvoidingView
      style={{ flex: 1 }}
      behavior={Platform.OS === 'ios' ? 'padding' : undefined}>
      <View
        style={{ paddingHorizontal: 16, paddingVertical: 8, borderBottomWidth: 1, borderBottomColor: palette.border }}>
        <Text
          style={[styles.rowTitle, { color: palette.fg }]}
          numberOfLines={1}
          accessibilityLabel="File conversation path">
          {props.path}
        </Text>
        {snapshot && (
          <Text
            style={[styles.rowSub, { color: palette.muted }]}
            accessibilityLabel="File conversation status">
            {snapshot.status}{snapshot.streaming ? ' ●' : ''}
          </Text>
        )}
        {contextLine !== null && (
          <Text
            style={[styles.rowSub, { color: palette.muted }]}
            accessibilityLabel="File conversation context">
            {contextLine}
          </Text>
        )}
      </View>
      {preview && preview.lines.length > 0 && (
        <SourceExcerpt
          preview={preview}
          palette={palette}
          selection={selection}
          onSelectLine={onSelectLine}
        />
      )}
      <ScrollView style={styles.list} keyboardShouldPersistTaps="handled">
        {props.unreadable && (
          <Text
            style={[styles.emptyText, { color: palette.muted, padding: 24 }]}
            accessibilityLabel="File conversation unreadable">
            File conversation snapshot is unreadable.
          </Text>
        )}
        {!props.unreadable && entries.length === 0 && (
          <Text
            style={[styles.emptyText, { color: palette.muted, padding: 24 }]}
            accessibilityLabel="File conversation empty">
            No messages yet about this file.
          </Text>
        )}
        {entries.map((entry) => (
          <FileConversationEntryRow key={entry.id} entry={entry} palette={palette} />
        ))}
        <View style={styles.bottomPad} />
      </ScrollView>
      {selection && (
        <View
          style={{ flexDirection: 'row', alignItems: 'center', paddingHorizontal: 16, paddingVertical: 4 }}
          accessibilityLabel="Selected lines">
          <Text style={[styles.rowSub, { color: palette.fg, flex: 1 }]}>
            Selected lines {selection.startLine}–{selection.endLine}
          </Text>
          {props.onBlame && (
            <TouchableOpacity
              onPress={props.onBlame}
              disabled={props.blamePending === true}
              accessibilityLabel="Blame selected lines"
              style={{ minHeight: 44, justifyContent: 'center', paddingHorizontal: 8 }}>
              <Text style={{ color: palette.accent, fontSize: 14 }}>Blame</Text>
            </TouchableOpacity>
          )}
          <TouchableOpacity
            onPress={() => props.onSelectionChange(null)}
            accessibilityLabel="Clear selection">
            <Text style={{ color: palette.error, fontSize: 18, paddingHorizontal: 10 }}>×</Text>
          </TouchableOpacity>
        </View>
      )}
      {props.blamePending === true && (
        <Text
          style={[styles.rowSub, { color: palette.muted, paddingHorizontal: 16, minHeight: 20 }]}
          accessibilityLabel="Blame loading">
          Blame…
        </Text>
      )}
      {props.blame && props.blamePending !== true && (
        <View accessibilityLabel="Blame attribution" style={{ paddingHorizontal: 16, paddingBottom: 4 }}>
          {props.blame.lines.map((line) => (
            <Text
              key={line.currentLine}
              style={[styles.rowSub, { color: palette.muted, fontFamily: palette.mono }]}
              numberOfLines={1}>
              {blameAttributionLabel(line)}
            </Text>
          ))}
        </View>
      )}
      <View style={[styles.composerRow, { backgroundColor: palette.surface, borderTopColor: palette.border }]}>
        <TextInput
          ref={composerRef}
          style={[styles.sideChatComposerInput, { backgroundColor: palette.bg, borderColor: palette.border, color: palette.fg }]}
          value={props.draft}
          onChangeText={props.onDraftChange}
          onKeyPress={onComposerKey}
          placeholder={streaming ? 'Waiting…' : 'Message'}
          placeholderTextColor={palette.muted}
          multiline
          scrollEnabled
          blurOnSubmit={false}
          accessibilityLabel="File conversation composer"
        />
        {streaming ? (
          <TouchableOpacity
            style={[styles.primaryButton, { backgroundColor: palette.error, marginTop: 0, paddingVertical: 10, paddingHorizontal: 14 }, props.busy && styles.faded]}
            disabled={props.busy}
            onPress={props.onStop}
            accessibilityLabel="Stop">
            <Text style={[styles.primaryButtonText, { color: palette.bg }]}>■ Stop</Text>
          </TouchableOpacity>
        ) : (
          <TouchableOpacity
            style={[styles.primaryButton, { backgroundColor: palette.accent, marginTop: 0, paddingVertical: 10, paddingHorizontal: 14 }, (props.draft.trim() === '' || props.busy) && styles.faded]}
            disabled={props.draft.trim() === '' || props.busy}
            onPress={props.onSend}
            accessibilityLabel="Send">
            <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Send</Text>
          </TouchableOpacity>
        )}
      </View>
    </KeyboardAvoidingView>
  );
}
