import React, { useMemo, useRef, useState } from 'react';
import { Alert, Linking, Modal, Share, Text, TouchableOpacity, View } from 'react-native';
import { WebView } from 'react-native-webview';
import { externalLinkPrompt, parseMarkdown, type MdBlock, type MdInline, type MdTableAlign } from './markdown';
import {
  MERMAID_DOWNLOAD_FILE_NAME,
  mermaidSourceWithinBound,
  mermaidViewerHtml,
  sanitizeMermaidSvg,
  viewerDiagramInjection,
  viewerNavigationAllowed,
  viewerNoteScript,
} from './mermaidSvg';
import { redactSecrets } from './redact';
import { AppBar, styles, type Palette } from './ui';

/** The viewer's WebView surface (cast like TmuxTerminalScreen's terminal
 *  WebView: react-native-webview's default type parameter makes its own props
 *  `never` under a generic ref). */
interface ViewerHandle {
  injectJavaScript: (script: string) => void;
}

const ViewerWebView = WebView as unknown as React.ComponentType<{
  ref?: React.Ref<ViewerHandle | null>;
  originWhitelist: string[];
  source: { html: string; baseUrl: string };
  allowFileAccess: boolean;
  setSupportMultipleWindows: boolean;
  onShouldStartLoadWithRequest: (event: { nativeEvent: { url: string } }) => boolean;
  onMessage: (event: { nativeEvent: { data: string } }) => void;
  style: { flex: number; backgroundColor: string };
}>;

/** External links are never opened on tap: the destination is named in a
 *  native confirmation whose default action is Cancel (mirrors the desktop
 *  shell's external-open prompt). */
function requestExternalOpen(href: string): void {
  const prompt = externalLinkPrompt(href);
  Alert.alert(
    prompt.title,
    prompt.message,
    [
      { text: 'Cancel', style: 'cancel' },
      { text: 'Open in browser', onPress: () => { void Linking.openURL(href); } },
    ],
    { cancelable: true },
  );
}

function MermaidBlock(props: { source: string; palette: Palette }): React.JSX.Element {
  const [open, setOpen] = useState(false);
  const [svg, setSvg] = useState('');
  const webRef = useRef<ViewerHandle | null>(null);
  const pendingDownloadRef = useRef(false);
  const withinBound = mermaidSourceWithinBound(props.source);

  const download = async (raw: string) => {
    const clean = sanitizeMermaidSvg(raw);
    if (!clean) return;
    try {
      await Share.share({ title: MERMAID_DOWNLOAD_FILE_NAME, message: clean });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      if (message === 'User did not share') return;
    }
  };

  return (
    <View style={[styles.card, { backgroundColor: props.palette.bg, borderColor: props.palette.border, marginHorizontal: 0 }]}>
      <Text style={[styles.cardMeta, { color: props.palette.accent }]}>Mermaid</Text>
      <Text style={[styles.cardPre, { color: props.palette.fg, fontFamily: props.palette.mono }]}>{props.source}</Text>
      {withinBound ? (
        <View style={{ flexDirection: 'row', gap: 12, marginTop: 8 }}>
          <TouchableOpacity onPress={() => { pendingDownloadRef.current = false; setSvg(''); setOpen(true); }} accessibilityLabel="View">
            <Text style={{ color: props.palette.accent }}>View</Text>
          </TouchableOpacity>
          <TouchableOpacity
            onPress={() => {
              if (svg) void download(svg);
              else {
                pendingDownloadRef.current = true;
                setOpen(true);
              }
            }}
            accessibilityLabel="Download SVG">
            <Text style={{ color: props.palette.accent }}>Download SVG</Text>
          </TouchableOpacity>
        </View>
      ) : (
        <Text style={[styles.cardMeta, { color: props.palette.muted }]}>Mermaid · source fallback</Text>
      )}
      <Modal visible={open} animationType="slide" onRequestClose={() => setOpen(false)}>
        <View style={[styles.screen, { backgroundColor: props.palette.bg }]}>
          <AppBar
            palette={props.palette}
            title="Mermaid"
            onBack={() => setOpen(false)}
            actions={[{
              icon: 'file-download',
              label: 'Download SVG',
              disabled: svg === '',
              onPress: () => { void download(svg); },
            }]}
          />
          <ViewerWebView
            ref={webRef}
            // '*' is deliberate: this library hands every URL that FAILS
            // originWhitelist to Linking.openURL with no consent
            // (react-native-webview WebViewShared createOnShouldStartLoadWithRequest),
            // so the single decision point must be the callback below.
            originWhitelist={['*']}
            source={{ html: mermaidViewerHtml(props.source), baseUrl: 'file:///android_asset/' }}
            // Android keeps file:///android_asset reachable with file access
            // off, so the bundled mermaid.min.js still loads.
            allowFileAccess={false}
            setSupportMultipleWindows={false}
            onShouldStartLoadWithRequest={(event) => viewerNavigationAllowed(event.nativeEvent.url)}
            onMessage={(event) => {
              let frame: unknown;
              try {
                frame = JSON.parse(event.nativeEvent.data);
              } catch {
                return;
              }
              if (!frame || typeof frame !== 'object') return;
              const row = frame as { type?: string; svg?: string };
              if (row.type !== 'svg' || typeof row.svg !== 'string') return;
              // Sanitize BEFORE injecting: the page only innerHTMLs what RN
              // hands it, and RN only hands sanitized SVG.
              const injection = viewerDiagramInjection(row.svg);
              if (injection.script === '') {
                pendingDownloadRef.current = false;
                webRef.current?.injectJavaScript(viewerNoteScript('Diagram rejected by the SVG sanitizer'));
                return;
              }
              setSvg(injection.clean);
              webRef.current?.injectJavaScript(injection.script);
              if (pendingDownloadRef.current) {
                pendingDownloadRef.current = false;
                void download(injection.clean);
              }
            }}
            style={{ flex: 1, backgroundColor: props.palette.bg }}
          />
        </View>
      </Modal>
    </View>
  );
}

function InlineRun(props: { nodes: MdInline[]; palette: Palette; baseStyle: object }): React.JSX.Element {
  return (
    <Text style={props.baseStyle}>
      {props.nodes.map((node, index) => {
        if (node.type === 'strong') {
          return <Text key={index} style={{ fontWeight: '700', color: props.palette.fg }}>{node.text}</Text>;
        }
        if (node.type === 'em') {
          return <Text key={index} style={{ fontStyle: 'italic', color: props.palette.fg }}>{node.text}</Text>;
        }
        if (node.type === 'code') {
          return <Text key={index} style={{ fontFamily: props.palette.mono, color: props.palette.accent }}>{node.text}</Text>;
        }
        if (node.type === 'link') {
          return (
            <Text
              key={index}
              style={{ color: props.palette.accent }}
              onPress={() => requestExternalOpen(node.href)}>
              {node.text}
            </Text>
          );
        }
        return <Text key={index} style={{ color: props.palette.fg }}>{node.text}</Text>;
      })}
    </Text>
  );
}

function TableRow(props: { cells: MdInline[][]; align: MdTableAlign[]; palette: Palette; head?: boolean; last?: boolean }): React.JSX.Element {
  return (
    <View style={[styles.tableRow, { borderColor: props.palette.border }, !props.last && { borderBottomWidth: 1 }]}>
      {props.cells.map((cell, index) => (
        <View
          key={index}
          style={[
            styles.tableCell,
            { borderColor: props.palette.border },
            index === props.cells.length - 1 && styles.tableCellLast,
          ]}>
          <InlineRun
            nodes={cell}
            palette={props.palette}
            baseStyle={[
              props.head ? styles.tableHeadText : styles.tableText,
              {
                color: props.head ? props.palette.fg : props.palette.muted,
                textAlign: props.align[index] ?? 'left',
              },
            ]}
          />
        </View>
      ))}
    </View>
  );
}

function TableBlock(props: { block: Extract<MdBlock, { type: 'table' }>; palette: Palette }): React.JSX.Element {
  const { align, header, rows } = props.block;
  return (
    <View style={[styles.table, { borderColor: props.palette.border }]}>
      <TableRow cells={header} align={align} palette={props.palette} head />
      {rows.map((row, index) => (
        <TableRow
          key={index}
          cells={row}
          align={align}
          palette={props.palette}
          last={index === rows.length - 1}
        />
      ))}
    </View>
  );
}

export function MarkdownBody(props: { text: string; palette: Palette }): React.JSX.Element {
  const blocks = useMemo(() => parseMarkdown(redactSecrets(props.text)), [props.text]);
  return (
    <View>
      {blocks.map((block, index) => {
        if (block.type === 'table') {
          return <TableBlock key={index} block={block} palette={props.palette} />;
        }
        if (block.type === 'mermaid') {
          return <MermaidBlock key={index} source={block.body} palette={props.palette} />;
        }
        if (block.type === 'code') {
          return (
            <Text key={index} style={[styles.cardPre, { color: props.palette.fg, fontFamily: props.palette.mono }]}>
              {block.body}
            </Text>
          );
        }
        if (block.type === 'hr') {
          return <View key={index} style={{ height: 1, backgroundColor: props.palette.border, marginVertical: 8 }} />;
        }
        if (block.type === 'h') {
          return (
            <InlineRun
              key={index}
              nodes={block.inlines}
              palette={props.palette}
              baseStyle={[styles.rowTitle, { color: props.palette.fg, fontWeight: '700', fontSize: 18 - Math.min(block.level, 3) }]}
            />
          );
        }
        if (block.type === 'li') {
          return (
            <InlineRun
              key={index}
              nodes={[{ type: 'text', text: block.ordered ? '1. ' : '• ' }, ...block.inlines]}
              palette={props.palette}
              baseStyle={[styles.rowTitle, { color: props.palette.fg }]}
            />
          );
        }
        if (block.type === 'quote') {
          return (
            <View key={index} style={{ borderLeftWidth: 2, borderLeftColor: props.palette.accent, paddingLeft: 8 }}>
              <InlineRun nodes={block.inlines} palette={props.palette} baseStyle={[styles.rowTitle, { color: props.palette.muted }]} />
            </View>
          );
        }
        return (
          <InlineRun
            key={index}
            nodes={block.inlines}
            palette={props.palette}
            baseStyle={[styles.rowTitle, { color: props.palette.fg }]}
          />
        );
      })}
    </View>
  );
}
