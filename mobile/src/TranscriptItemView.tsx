import React, { useEffect, useMemo, useState } from 'react';
import { Clipboard, Text, TouchableOpacity, View } from 'react-native';
import { MarkdownBody } from './MarkdownBody';
import { normalizeThinkingNewlines } from './markdown';
import { redactSecrets } from './redact';
import type { ContentBlock, TranscriptItem } from './transcriptItems';
import { styles, type Palette } from './ui';

const TOOL_ARGS_LINE_LIMIT = 6;
const TOOL_RESULT_LINE_LIMIT = 8;
const BASH_OUTPUT_LINE_LIMIT = 10;
const COPY_FLASH_MS = 1200;

// Card titles for well-known wire tool names (keyed by lowercase name).
const KNOWN_TOOL_TITLES: Record<string, string> = {
  task: 'Task',
  todo: 'Todo',
  edit: 'Edit',
  bash: 'Command',
  process: 'Process',
  read: 'Read',
  write: 'Write',
  hub: 'Hub',
  irc: 'IRC',
  web_search: 'Web Search',
  image_generate: 'Image Generate',
  generate_image: 'Image Generate',
  browser: 'Browser',
  ask: 'Ask',
  glob: 'Glob',
  grep: 'Grep',
  lsp: 'LSP',
};

const TOOL_TITLE_ACRONYMS: Record<string, true> = {
  IRC: true,
  RPC: true,
  LSP: true,
  URL: true,
  HTTP: true,
  JSON: true,
  SQL: true,
  API: true,
  ID: true,
};

/** Visible tool-card title for a wire tool name; the wire value itself stays
 *  in the card meta line. Known tools get their fixed title, anything else
 *  keeps its raw name Title-Cased with whole-word acronyms preserved.
 *  Redaction runs BEFORE the split so `sk-…`-shaped secrets survive intact. */
export function humanToolTitle(raw: unknown): string {
  const name = redactSecrets(raw == null ? '' : String(raw)).trim();
  if (name === '') return '';
  const known = KNOWN_TOOL_TITLES[name.toLowerCase()];
  if (known !== undefined) return known;
  return name
    .split(/[_\-\s]+/)
    .filter((part) => part !== '')
    .map((part) => {
      const upper = part.toUpperCase();
      return TOOL_TITLE_ACRONYMS[upper] === true ? upper : part.charAt(0).toUpperCase() + part.slice(1);
    })
    .join(' ');
}

function CopyAction(props: { label: string; text: string; palette: Palette }): React.JSX.Element {
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    setCopied(false);
  }, [props.text]);
  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(false), COPY_FLASH_MS);
    return () => clearTimeout(timer);
  }, [copied]);
  return (
    <TouchableOpacity
      accessibilityLabel={props.label}
      onPress={() => {
        // Core Clipboard is the only zero-dependency clipboard; redact here.
        Clipboard.setString(redactSecrets(props.text));
        setCopied(true);
      }}>
      <Text style={[styles.cardChip, { color: props.palette.accent, borderColor: props.palette.border }]}>
        {copied ? 'copied' : 'copy'}
      </Text>
    </TouchableOpacity>
  );
}

/** Mono body whose default view is a bounded line preview; the fold reveals
 *  the whole payload instead of clipping it behind a fixed line count. */
function LinesFold(props: { text: string; limit: number; field: string; color: string; palette: Palette }): React.JSX.Element {
  const [open, setOpen] = useState(false);
  const lines = useMemo(() => props.text.split('\n'), [props.text]);
  const hidden = lines.length - props.limit;
  const collapsed = hidden > 0 && !open;
  const body = collapsed
    ? `${lines.slice(0, props.limit).join('\n')}\n… ${hidden} more ${hidden === 1 ? 'line' : 'lines'}`
    : props.text;
  return (
    <View>
      <Text style={[styles.cardPre, { color: props.color, fontFamily: props.palette.mono }]}>{body}</Text>
      {hidden > 0 && (
        <TouchableOpacity
          accessibilityLabel={`${open ? 'Collapse' : 'Expand'} ${props.field}`}
          onPress={() => setOpen((value) => !value)}>
          <Text style={[styles.cardMeta, { color: props.palette.accent }]}>
            {open ? '▾ collapse' : `▸ ${hidden} more lines`}
          </Text>
        </TouchableOpacity>
      )}
    </View>
  );
}

function ThinkingFold(props: { thinking: string; palette: Palette }): React.JSX.Element {
  const [open, setOpen] = useState(false);
  return (
    <View>
      <TouchableOpacity onPress={() => setOpen((value) => !value)} accessibilityLabel="Thinking">
        <Text style={[styles.cardMeta, { color: props.palette.muted }]}>{open ? '▾ Thinking' : '▸ Thinking'}</Text>
      </TouchableOpacity>
      {open && (
        <View style={{ padding: 8 }}>
          <MarkdownBody text={normalizeThinkingNewlines(props.thinking)} palette={props.palette} />
        </View>
      )}
    </View>
  );
}

function AssistantBlocks(props: { blocks: ContentBlock[]; palette: Palette }): React.JSX.Element {
  return (
    <View>
      {props.blocks.map((block, index) => {
        if (block.type === 'thinking' && block.thinking) {
          return <ThinkingFold key={index} thinking={block.thinking} palette={props.palette} />;
        }
        if (block.type === 'text' && block.text) {
          return <MarkdownBody key={index} text={block.text} palette={props.palette} />;
        }
        return null;
      })}
    </View>
  );
}

export function TranscriptItemView(props: { item: TranscriptItem; palette: Palette }): React.JSX.Element {
  const { item, palette } = props;
  switch (item.kind) {
    case 'user':
      return (
        <View style={[styles.bubbleUser, { backgroundColor: palette.surface, borderColor: palette.border }]}>
          <MarkdownBody text={item.text} palette={palette} />
        </View>
      );
    case 'assistant':
      return (
        <View style={[styles.bubbleRunner, { backgroundColor: palette.surface, borderColor: palette.border }]}>
          <AssistantBlocks blocks={item.blocks} palette={palette} />
        </View>
      );
    case 'tool':
      return (
        <View style={[styles.card, { backgroundColor: palette.surface, borderColor: palette.border }]} accessibilityLabel={`Tool ${item.toolName}`}>
          <View style={styles.cardHead}>
            <Text style={[styles.cardTitle, { color: palette.fg }]}>{humanToolTitle(item.toolName)}</Text>
            {item.result !== '' && <CopyAction label="Copy result" text={item.result} palette={palette} />}
          </View>
          <Text style={[styles.cardMeta, { color: palette.muted }]}>{item.toolName} · {item.status}</Text>
          {item.args !== '' && (
            <LinesFold text={item.args} limit={TOOL_ARGS_LINE_LIMIT} field="raw args" color={palette.muted} palette={palette} />
          )}
          {item.result !== '' && (
            <LinesFold text={item.result} limit={TOOL_RESULT_LINE_LIMIT} field="result" color={palette.fg} palette={palette} />
          )}
        </View>
      );
    case 'bash':
      return (
        <View style={[styles.card, { backgroundColor: palette.surface, borderColor: palette.border }]} accessibilityLabel="Bash">
          <View style={styles.cardHead}>
            <Text style={[styles.cardTitle, { color: palette.fg, fontFamily: palette.mono }]} numberOfLines={2}>{item.command}</Text>
            {item.output !== '' && <CopyAction label="Copy output" text={item.output} palette={palette} />}
          </View>
          {item.status && <Text style={[styles.cardMeta, { color: palette.muted }]}>{item.status}</Text>}
          <LinesFold text={item.output} limit={BASH_OUTPUT_LINE_LIMIT} field="output" color={palette.fg} palette={palette} />
        </View>
      );
    case 'notice':
      return <Text style={[styles.emptyText, { color: palette.muted, padding: 12 }]}>{item.text}</Text>;
  }
}
