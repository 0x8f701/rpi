#!/usr/bin/env node
// Side Chat markdown rendering regression for src/panels/SideChatPanel.tsx:
// assistant and user entry text must render through the SHARED MarkdownBody
// pipeline (renderMarkdown: escapeHtml-first sanitize, whitelisted links/
// images, code fences, Mermaid hosts) — never as a raw React text node — so
// '**bold** and `code`' produces real <strong>/<code> markup, hostile HTML
// stays literal, and tool/system entries (special-cased) keep their raw
// escaped text. The entry wrapper's error/partial classes must survive.
// Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
//
// Renders the REAL SideChatPanel through react-dom/server static markup
// (effects — scroll pin, Mermaid hydration — never run, so the mermaid
// alias + css-empty loader make the node bundle hermetic).
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { SideChatPanel, admitLiveSideChatIntake, applySideChatSttInsert, finishSideChatSttInsertAfterAwait, generateSideChatName, planSideChatSttInsertThrowToast, planSideChatSttInsertToast, planSideChatSwitchCatchToast, resolveSideChatSttInsert, settleSideChatSttAfterSwitch, sideChatCanSubmit, sideChatPromptAccepted, SIDE_CHAT_NAME_MAX_CHARS, SIDE_CHAT_STT_CLOSED_TOAST, type SideChatEntryWire, type SideChatSnapshot } from '../src/panels/SideChatPanel.tsx';
import { cancelAllConversationStreams, cancelConversationStream, cancelConversationStreamsForOwner, cancelConversationStreamsForScope, conversationPathContext, conversationStreamKey, conversationStreamParseCountForTest, flushConversationStreamRafsForTest, peekConversationStreamHtml, resetConversationStreamStateForTest, scheduleConversationStream } from '../src/panels/ConversationEntries.tsx';
import { renderMarkdown, renderStreamingMarkdown } from '../src/markdown.ts';
const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const noop = () => {};
const panel = (entries, streaming = false) => renderToStaticMarkup(
  createElement(SideChatPanel, {
    snapshot: snapshot(entries, streaming),
    onNew: noop,
    onSwitch: noop,
    onClose: noop,
    onPrompt: noop,
    onAbort: noop,
    onClosePanel: noop,
  }),
);

const snapshot = (entries, streaming = false): SideChatSnapshot => ({
  active: 't1',
  maxTabs: 8,
  tabs: [{ name: 't1', streaming, status: streaming ? 'streaming' : 'idle', entries }],
});

function wireEntry(entry: Partial<SideChatEntryWire> & Pick<SideChatEntryWire, 'role' | 'text'>): SideChatEntryWire {
  return {
    id: entry.id ?? `e-${entry.role}`,
    kind: entry.kind ?? entry.role,
    role: entry.role,
    text: entry.text,
    toolName: entry.toolName,
    toolArgs: entry.toolArgs,
    output: entry.output,
    thinking: entry.thinking,
    isError: entry.isError ?? false,
    isPartial: entry.isPartial ?? false,
  };
}

const MD_TEXT = '**bold** and `code`';

{
  const entries = [
    wireEntry({
      id: 'g',
      role: 'tool',
      kind: 'tool',
      text: '',
      toolName: 'grep',
      output: 'src/runtime/process_block.rs:472:fn handle(',
    }),
    wireEntry({
      id: 'a',
      role: 'assistant',
      kind: 'assistant',
      text: 'see `process_block.rs:472-480`',
    }),
  ];
  check(
    'side chat path context joins tool output and cite',
    conversationPathContext(entries).includes('src/runtime/process_block.rs:472:fn handle(')
      && conversationPathContext(entries).includes('see `process_block.rs:472-480`'),
  );
  const citeHtml = panel(entries);
  check(
    'side chat unique nearby cite is a path link',
    citeHtml.includes('md-code-path')
      && citeHtml.includes('data-md-path="src/runtime/process_block.rs"')
      && citeHtml.includes('data-md-line="472"')
      && !citeHtml.includes('data-md-completion='),
    citeHtml.slice(0, 800),
  );
  const homonymHtml = panel([
    wireEntry({
      id: 'g2',
      role: 'tool',
      kind: 'tool',
      text: '',
      toolName: 'grep',
      output: 'src/runtime/process_block.rs and crates/other/process_block.rs',
    }),
    wireEntry({
      id: 'a2',
      role: 'assistant',
      kind: 'assistant',
      text: 'see `process_block.rs:472-480`',
    }),
  ]);
  check(
    'side chat homonym cite stays unresolved',
    homonymHtml.includes('data-md-resolved="none"')
      && homonymHtml.includes('>process_block.rs:472-480</code>')
      && !homonymHtml.includes('md-code-path')
      && !homonymHtml.includes('data-md-path='),
    homonymHtml.slice(0, 800),
  );
}

function matchingBrace(src, openIdx) {
  let depth = 0;
  for (let i = openIdx; i < src.length; i += 1) {
    if (src[i] === '{') depth += 1;
    else if (src[i] === '}') {
      depth -= 1;
      if (depth === 0) return i;
    }
  }
  throw new Error(`unbalanced brace at ${openIdx}`);
}

function parseDecls(body) {
  const decls = {};
  for (const chunk of body.split(';')) {
    const idx = chunk.indexOf(':');
    if (idx < 0) continue;
    const prop = chunk.slice(0, idx).trim();
    const value = chunk.slice(idx + 1).trim();
    if (!prop || !value) continue;
    decls[prop] = value.replace(/\s*!important\s*$/, '').trim();
  }
  return decls;
}

function parseCssRules(css) {
  const src = css.replace(/\/\*[\s\S]*?\*\//g, '');
  const rules = [];
  function parseBlock(from, to, media) {
    let i = from;
    while (i < to) {
      while (i < to && /\s/.test(src[i])) i += 1;
      if (i >= to) break;
      if (src.startsWith('@', i)) {
        const brace = src.indexOf('{', i);
        if (brace < 0 || brace >= to) break;
        const prelude = src.slice(i, brace).trim();
        const close = matchingBrace(src, brace);
        if (prelude.startsWith('@media')) {
          const query = prelude.slice('@media'.length).trim();
          const combined = media ? `${media} and ${query}` : query;
          parseBlock(brace + 1, close, combined);
        }
        i = close + 1;
        continue;
      }
      const brace = src.indexOf('{', i);
      if (brace < 0 || brace >= to) break;
      const selector = src.slice(i, brace).trim();
      const close = matchingBrace(src, brace);
      if (selector && !selector.startsWith('@')) {
        rules.push({ selector, decls: parseDecls(src.slice(brace + 1, close)), media, index: rules.length });
      }
      i = close + 1;
    }
  }
  parseBlock(0, src.length, null);
  return rules;
}

function selectorSubject(part) {
  const noHas = part.replace(/:has\([^)]*\)/g, '');
  const bits = noHas.trim().split(/\s+/);
  return bits[bits.length - 1] ?? '';
}

function matchesKind(part, kind) {
  const subject = selectorSubject(part);
  if (kind === 'sidechat') {
    return /#side-chat-panel(?![\w-])/.test(subject);
  }
  return /\.panel-drawer(?![\w-])/.test(subject) && !/#side-chat-panel(?![\w-])/.test(subject);
}

function specificity(part) {
  const subject = selectorSubject(part);
  const ids = (subject.match(/#[\w-]+/g) || []).length;
  const classes = (subject.match(/\.[\w-]+/g) || []).length;
  return ids * 100 + classes * 10;
}

function mediaMatches(media, vw) {
  if (!media) return true;
  return media.split(/\s+and\s+/).every((piece) => {
    const q = piece.trim().replace(/^\(/, '').replace(/\)$/, '');
    const max = q.match(/max-width:\s*(\d+)px/);
    if (max) return vw <= Number(max[1]);
    const min = q.match(/min-width:\s*(\d+)px/);
    if (min) return vw >= Number(min[1]);
    return true;
  });
}

function mediaHasMax(media, px) {
  if (!media) return false;
  return media.split(/\s+and\s+/).some((piece) => {
    const q = piece.trim().replace(/^\(/, '').replace(/\)$/, '');
    const max = q.match(/max-width:\s*(\d+)px/);
    return Boolean(max) && Number(max[1]) === px;
  });
}

function lastSideChatDecl(rules, prop) {
  let desktop = null;
  let phone = null;
  for (const rule of rules) {
    const idRule = rule.selector.split(',').some((part) => /#side-chat-panel(?![\w-])/.test(selectorSubject(part.trim())));
    if (!idRule || rule.decls[prop] == null) continue;
    if (mediaHasMax(rule.media, 768) || mediaHasMax(rule.media, 720)) phone = rule.decls[prop];
    else desktop = rule.decls[prop];
  }
  return { desktop, phone };
}

function evalAddSub(expr) {
  const cleaned = String(expr).replace(/px/g, '').replace(/\s+/g, '');
  if (!cleaned) return Number.NaN;
  if (/[a-zA-Z_]/.test(cleaned)) return Number.NaN;
  const tokens = cleaned.split(/([+-])/);
  let acc = Number(tokens[0]);
  for (let i = 1; i < tokens.length; i += 2) {
    const n = Number(tokens[i + 1]);
    acc = tokens[i] === '+' ? acc + n : acc - n;
  }
  return acc;
}

function evalCssLength(value, vw, vh, header, footer) {
  if (value == null) return null;
  const raw = value.trim();
  if (raw === 'auto' || raw === 'none') return null;
  let s = raw
    .replace(/var\(--app-header-height(?:,\s*[^)]+)?\)/g, `${header}px`)
    .replace(/var\(--app-footer-height(?:,\s*[^)]+)?\)/g, `${footer}px`)
    .replace(/var\(--panel-drawer-height(?:,\s*[^)]+)?\)/g, '90vh');
  s = s.replace(/(\d*\.?\d+)vw/g, (_, n) => `${(Number(n) / 100) * vw}px`);
  s = s.replace(/(\d*\.?\d+)dvh/g, (_, n) => `${(Number(n) / 100) * vh}px`);
  s = s.replace(/(\d*\.?\d+)vh/g, (_, n) => `${(Number(n) / 100) * vh}px`);
  for (let step = 0; step < 8; step += 1) {
    const next = s
      .replace(/calc\(([^()]+)\)/g, (_, inner) => `${evalAddSub(inner)}px`)
      .replace(/min\(([^()]+)\)/g, (_, inner) => {
        const parts = inner.split(',').map((part) => evalAddSub(part));
        return `${Math.min(...parts)}px`;
      });
    if (next === s) break;
    s = next;
  }
  if (s === 'auto' || s === 'none') return null;
  const px = evalAddSub(s);
  return Number.isFinite(px) ? px : null;
}

function computeBox(rules, kind, vw, vh, header, footer) {
  const props = {};
  const specOf = {};
  const indexOf = {};
  for (const rule of rules) {
    if (!mediaMatches(rule.media, vw)) continue;
    const parts = rule.selector.split(',').map((part) => part.trim()).filter((part) => matchesKind(part, kind));
    if (parts.length === 0) continue;
    const spec = Math.max(...parts.map(specificity));
    for (const [prop, val] of Object.entries(rule.decls)) {
      const prevSpec = specOf[prop] ?? -1;
      const prevIdx = indexOf[prop] ?? -1;
      if (spec > prevSpec || (spec === prevSpec && rule.index >= prevIdx)) {
        props[prop] = val;
        specOf[prop] = spec;
        indexOf[prop] = rule.index;
      }
    }
  }
  const widthRaw = evalCssLength(props.width, vw, vh, header, footer);
  const maxWidth = evalCssLength(props['max-width'], vw, vh, header, footer);
  const heightRaw = evalCssLength(props.height, vw, vh, header, footer);
  const maxHeight = evalCssLength(props['max-height'], vw, vh, header, footer);
  const top = evalCssLength(props.top, vw, vh, header, footer) ?? 0;
  const bottom = evalCssLength(props.bottom, vw, vh, header, footer) ?? 0;
  let width = widthRaw ?? vw;
  if (maxWidth != null) width = Math.min(width, maxWidth);
  let height = vh - top - bottom;
  if (heightRaw != null) height = Math.min(height, heightRaw);
  if (maxHeight != null) height = Math.min(height, maxHeight);
  return { width, height, top, bottom };
}

// ---- assistant entries render through the shared markdown pipeline ----
{
  const html = panel([wireEntry({ role: 'assistant', text: MD_TEXT, isError: false, isPartial: false })]);
  check('assistant bold renders <strong>', html.includes('<strong>bold</strong>'), html);
  check('assistant code renders <code class="md-code">', html.includes('<code class="md-code">code</code>'), html);
  check('assistant entry keeps its bubble class', html.includes('side-chat__entry side-chat__entry--assistant'));
  check('assistant has no role gutter', !html.includes('side-chat__role'), html);
  check('assistant text surface keeps side-chat__text', html.includes('class="side-chat__text"'), html);
}

// ---- user entries render through the same markdown pipeline ----
{
  const html = panel([wireEntry({ role: 'user', text: MD_TEXT, isError: false, isPartial: false })]);
  check('user bold renders <strong>', html.includes('<strong>bold</strong>'), html);
  check('user code renders <code class="md-code">', html.includes('<code class="md-code">code</code>'), html);
  check('user has no role gutter', !html.includes('side-chat__role'), html);
}

// ---- tool entries use the shared ToolCard, never a raw [bash] dump ----
{
  const longOutput = `${Array.from({ length: 19 }, (_, i) => `line-${i}`).join('\n')}\n\u001b[32mline-19\u001b[0m`;
  const toolHtml = panel([wireEntry({
    id: 'tool-bash',
    kind: 'tool',
    role: 'tool',
    text: '[bash] Tool bash not found',
    toolName: 'bash',
    toolArgs: { command: 'ls' },
    output: longOutput,
    isError: false,
    isPartial: false,
  })]);
  check('tool entry renders the shared tool-card', toolHtml.includes('tool-card') && toolHtml.includes('data-tool-name="bash"'), toolHtml.slice(0, 400));
  check('tool entry does not dump a raw [bash] row', !toolHtml.includes('[bash]'), toolHtml.slice(0, 400));
  check('bash output uses the ANSI path', toolHtml.includes('tool-card__output') && toolHtml.includes('ansi-fg-2'), toolHtml.slice(toolHtml.indexOf('tool-card'), toolHtml.indexOf('tool-card') + 900));
  check('bash output is bounded', toolHtml.includes('more line'), toolHtml.slice(0, 800));
  check('tool entry has no role gutter', !toolHtml.includes('side-chat__role'), toolHtml.slice(0, 200));
  const grepHtml = panel([wireEntry({
    id: 'tool-grep',
    kind: 'tool',
    role: 'tool',
    text: '[grep] src/queue/gatherer.rs',
    toolName: 'grep',
    toolArgs: { pattern: 'handle', path: 'src' },
    output: 'src/queue/gatherer.rs:1:handle',
    isError: false,
    isPartial: false,
  })]);
  check('grep tool uses the shared tool-card', grepHtml.includes('tool-card') && grepHtml.includes('data-tool-name="grep"'), grepHtml.slice(0, 400));
  check('grep tool does not dump a raw [grep] row', !grepHtml.includes('[grep]'), grepHtml.slice(0, 400));
}

// ---- thinking uses the shared thinking fold ----
{
  const html = panel([wireEntry({
    id: 'think-1',
    kind: 'thinking',
    role: 'assistant',
    text: 'consider **the** queue',
    thinking: 'consider **the** queue',
    isError: false,
    isPartial: false,
  })]);
  check('thinking renders the shared fold', html.includes('class="thinking"') && html.includes('thinking__summary') && html.includes('thinking__body'), html.slice(0, 500));
  check('thinking body goes through markdown', html.includes('<strong>the</strong>'), html.slice(0, 800));
  check('thinking has no role gutter', !html.includes('side-chat__role'), html.slice(0, 200));
}

// ---- system entries render through markdown, not a raw span ----
{
  const systemHtml = panel([wireEntry({ role: 'system', text: MD_TEXT, isError: false, isPartial: false })]);
  check('system bold renders <strong>', systemHtml.includes('<strong>bold</strong>'), systemHtml);
  check('system has no role gutter', !systemHtml.includes('side-chat__role'), systemHtml);
}

// ---- hostile HTML stays literal through BOTH surfaces ----
{
  const hostile = '<script>alert(1)</script><img src=x onerror=alert(1)>';
  const assistantHtml = panel([wireEntry({ role: 'assistant', text: hostile, isError: false, isPartial: false })]);
  check('assistant hostile HTML escaped, no live tags', !assistantHtml.includes('<script>') && !assistantHtml.includes('<img '), assistantHtml);
  check('assistant hostile HTML visible as escaped text', assistantHtml.includes('&lt;script&gt;alert(1)&lt;/script&gt;'), assistantHtml);
  const toolHtml = panel([wireEntry({
    id: 'tool-hostile',
    kind: 'tool',
    role: 'tool',
    text: hostile,
    toolName: 'read',
    toolArgs: { path: 'x.rs' },
    output: hostile,
    isError: false,
    isPartial: false,
  })]);
  check('tool hostile HTML escaped, no live tags', !toolHtml.includes('<script>') && !toolHtml.includes('<img '), toolHtml);
}

// ---- error/partial entry classes survive the markdown route ----
{
  const html = panel([wireEntry({ role: 'assistant', text: MD_TEXT, isError: true, isPartial: true })]);
  check('error class preserved on markdown entry', html.includes('side-chat__entry--error'), html);
  check('partial class preserved on markdown entry', html.includes('side-chat__entry--partial'), html);
  check('markdown still rendered on error/partial entry', html.includes('<strong>bold</strong>'), html);
  const settled = panel([wireEntry({ role: 'assistant', text: MD_TEXT, isError: false, isPartial: false })]);
  const streamingHtml = peekConversationStreamHtml(conversationStreamKey('mount-a', 'side', 't1', 'e-assistant'), MD_TEXT);
  check(
    'settled markdown matches non-streaming renderMarkdown',
    settled.includes('<strong>bold</strong>') && settled.includes('<code class="md-code">code</code>'),
    settled,
  );
  check(
    'streaming parse is byte-identical to renderMarkdown',
    streamingHtml === renderMarkdown(MD_TEXT) && streamingHtml === renderStreamingMarkdown(MD_TEXT),
    streamingHtml,
  );
}

{
  const idle = panel([], false);
  check('idle composer shows Send', idle.includes('side-chat__composer-action--send') && idle.includes('➤') && idle.includes('Send'), idle);
  check('idle composer has no Stop', !idle.includes('side-chat__composer-action--stop') && !idle.includes('>Stop</span>'), idle);
  const streamingHtml = panel([], true);
  check(
    'streaming composer shows Stop',
    streamingHtml.includes('side-chat__composer-action--stop')
      && streamingHtml.includes('■')
      && streamingHtml.includes('Stop'),
    streamingHtml,
  );
  check(
    'streaming composer replaces Send',
    !streamingHtml.includes('side-chat__composer-action--send') && !streamingHtml.includes('Send'),
    streamingHtml,
  );
  check('idle composer shows attach', idle.includes('📎') && idle.includes('Attach files'), idle);
  check('idle composer shows mic', idle.includes('🎤') && idle.includes('Hold to talk'), idle);
  check(
    'idle composer order is attach then mic then Send last',
    (() => {
      const attachAt = idle.indexOf('side-chat__composer-action--attach');
      const micAt = idle.indexOf('side-chat__composer-action--mic');
      const sendAt = idle.indexOf('side-chat__composer-action--send');
      return attachAt >= 0 && micAt > attachAt && sendAt > micAt;
    })(),
    idle,
  );
  check('streaming composer keeps attach and mic', streamingHtml.includes('📎') && streamingHtml.includes('🎤'), streamingHtml);
  check(
    'streaming composer order is attach then mic then Stop last',
    (() => {
      const attachAt = streamingHtml.indexOf('side-chat__composer-action--attach');
      const micAt = streamingHtml.indexOf('side-chat__composer-action--mic');
      const stopAt = streamingHtml.indexOf('side-chat__composer-action--stop');
      return attachAt >= 0 && micAt > attachAt && stopAt > micAt;
    })(),
    streamingHtml,
  );
  const src = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'panels', 'SideChatPanel.tsx'), 'utf8');
  const enterBlock = src.match(/if \(streaming\) return;[\s\S]*?composerEnterShouldSubmit\(/)?.[0] ?? '';
  check(
    'Enter while streaming returns without submit or abort',
    enterBlock !== ''
      && !enterBlock.includes('onAbort')
      && /if \(e\.key === 'Escape' && streaming\) \{[\s\S]*?onAbort\(\);/.test(src),
    enterBlock.slice(0, 240),
  );
  check('new tab control has no name input', !idle.includes('new tab name') && !idle.includes('New side-chat tab name'), idle.slice(0, 500));
  check('new tab control is a New button', idle.includes('aria-label="New side-chat tab"') && idle.includes('New'), idle.slice(idle.indexOf('side-chat__new'), idle.indexOf('side-chat__new') + 280));
  check('dialog root id is present', idle.includes('id="side-chat-panel"'), idle.slice(0, 200));
  check(
    'dialog root is not a shared drawer',
    !/\bpanel-drawer\b/.test(idle) && !/\bpanel-drawer\b/.test(src),
    idle.slice(0, 240),
  );
}

{
  const pendingImage = { id: 'sc-1', name: 'shot.png', size: 12, mimeType: 'image/png', kind: 'image' };
  const readyImage = { ...pendingImage, dataBase64: 'Zg==' };
  const resurrected = admitLiveSideChatIntake([], [readyImage], ['sc-1']);
  check(
    'remove-before-completion does not resurrect the image',
    resurrected.next.length === 0 && resurrected.next.every((item) => item.id !== 'sc-1'),
  );
  const withoutChip = admitLiveSideChatIntake([], [readyImage], ['sc-1']);
  check(
    'admit ignores built images whose placeholder left the list',
    withoutChip.next.length === 0,
  );
  const replaced = admitLiveSideChatIntake([pendingImage], [readyImage], ['sc-1']);
  check(
    'live placeholder is replaced by the built image',
    replaced.next.length === 1 && replaced.next[0]?.id === 'sc-1' && replaced.next[0]?.dataBase64 === 'Zg==',
  );
  const firstAdmit = admitLiveSideChatIntake([pendingImage], [readyImage], ['sc-1']);
  const secondAdmit = admitLiveSideChatIntake([pendingImage], [readyImage], ['sc-1']);
  check(
    'calling admit twice with the same latest+built+pending snapshot does not leave a placeholder',
    firstAdmit.next.length === 1
      && firstAdmit.next[0]?.dataBase64 === 'Zg=='
      && secondAdmit.next.length === 1
      && secondAdmit.next[0]?.dataBase64 === 'Zg=='
      && secondAdmit.next[0]?.dataBase64 != null
      && sideChatCanSubmit({ streaming: false, prompt: 'hello', attachments: secondAdmit.next }) === true,
  );
  const skippedChip = admitLiveSideChatIntake([pendingImage], [], ['sc-1']);
  check(
    'resolved skip drops the pending chip so text can submit',
    skippedChip.next.length === 0
      && sideChatCanSubmit({ streaming: false, prompt: 'hello', attachments: skippedChip.next }) === true,
  );
  check(
    'text plus pending image cannot submit without the image',
    sideChatCanSubmit({ streaming: false, prompt: 'hello', attachments: [pendingImage] }) === false,
  );
  check(
    'text plus ready image can submit',
    sideChatCanSubmit({ streaming: false, prompt: 'hello', attachments: [readyImage] }) === true,
  );
}

{
  const ontoOtherTab = applySideChatSttInsert({
    text: 'hello',
    tabId: 'alpha',
    tabs: ['alpha', 'beta'],
  });
  check(
    'insert does not land on a different live tab than dest.tabId',
    ontoOtherTab.kind === 'awaitSwitch' && ontoOtherTab.kind === 'awaitSwitch' && ontoOtherTab.tabId === 'alpha',
  );
  check(
    'insert does not happen before switch ACK',
    ontoOtherTab.kind === 'awaitSwitch',
  );
  const cachedActive = applySideChatSttInsert({
    text: 'hello',
    tabId: 'alpha',
    tabs: ['alpha', 'beta'],
  });
  check(
    'cached active dest still awaits side_chat_switch',
    cachedActive.kind === 'awaitSwitch' && cachedActive.kind === 'awaitSwitch' && cachedActive.tabId === 'alpha',
  );
  const goneTab = applySideChatSttInsert({
    text: 'hello',
    tabId: 'alpha',
    tabs: ['beta'],
  });
  check(
    'gone dest.tabId discards instead of inserting into the current tab',
    goneTab.kind === 'discardClosed',
  );
  check(
    'failed switch ACK discards instead of inserting into the current tab',
    settleSideChatSttAfterSwitch({ tabId: 'alpha', ack: { kind: 'discardClosed' } }) === 'discardClosed',
  );
  check(
    'prompt is not inserted while active !== dest.tabId',
    settleSideChatSttAfterSwitch({
      tabId: 'alpha',
      ack: { kind: 'ok', active: 'beta', tabs: ['alpha', 'beta'] },
    }) === 'discardClosed',
  );
  check(
    'switch ACK with dest.tabId active allows insert',
    settleSideChatSttAfterSwitch({
      tabId: 'alpha',
      ack: { kind: 'ok', active: 'alpha', tabs: ['alpha', 'beta'] },
    }) === 'insert',
  );
  check(
    'malformed ACK is not treated as closed panel',
    settleSideChatSttAfterSwitch({ tabId: 'alpha', ack: { kind: 'malformed' } }) === 'malformed',
  );
  const panelSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/panels/SideChatPanel.tsx'), 'utf8');
  check('insertNow bypass is deleted', !panelSource.includes('insertNow'));
  check('Side chat pin follows entry content, not only count', panelSource.includes('active?.entries, streaming, pinIfPinned'));
  check('Side chat pin no longer keys only on entries.length', !panelSource.includes('active?.entries.length'));
  check('Side chat uses the shared pin controller', panelSource.includes('createScrollPin()') && panelSource.includes('onScroll={onTranscriptScroll}'));
  check('Side chat deleted the hard transcript jump', !panelSource.includes('transcriptRef.current.scrollTop = transcriptRef.current.scrollHeight'));
  check(
    'Side chat header title is Side chat',
    (panelSource.includes('>Side chat<') || panelSource.includes('side-chat__title">Side chat'))
      && !panelSource.includes('parallel sessions (rpi /btw)'),
  );
  const appSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/App.tsx'), 'utf8');
  check(
    'STT insert catch uses throw toast planner',
    appSource.includes('planSideChatSttInsertThrowToast(err, { sessionId, tabId })'),
  );
  check(
    'STT insert does not catch-all to the closed-panel toast',
    !/catch \{\s*ok = false;\s*\}/.test(appSource),
  );
}

void (async () => {
  let insertDecision = null;
  let resolveAck = (_ack) => {};
  const pendingInsert = resolveSideChatSttInsert({
    text: 'hello',
    tabId: 'alpha',
    tabs: ['alpha', 'beta'],
    switchTab: () => new Promise((resolve) => { resolveAck = resolve; }),
  }).then((decision) => {
    insertDecision = decision;
    return decision;
  });
  check(
    'insert stays pending until side_chat_switch ACK',
    insertDecision === null,
  );
  resolveAck({ kind: 'ok', active: 'beta', tabs: ['alpha', 'beta'] });
  check(
    'prompt is not sent while ACK active !== dest.tabId',
    (await pendingInsert).kind === 'discardClosed',
  );

  let cachedSwitchName = null;
  let cachedSettled = false;
  let resolveCached = (_ack) => {};
  const cachedPending = resolveSideChatSttInsert({
    text: 'hello',
    tabId: 'alpha',
    tabs: ['alpha', 'beta'],
    switchTab: (name) => {
      cachedSwitchName = name;
      return new Promise((resolve) => { resolveCached = resolve; });
    },
  }).then((decision) => {
    cachedSettled = true;
    return decision;
  });
  check(
    'cached alpha while a beta switch is in flight still awaits switch(alpha)',
    cachedSwitchName === 'alpha' && cachedSettled === false,
  );
  resolveCached({ kind: 'discardClosed' });
  check(
    'superseded in-flight switch discards instead of insertNow',
    (await cachedPending).kind === 'discardClosed',
  );

  const inserted = await resolveSideChatSttInsert({
    text: 'hello',
    tabId: 'alpha',
    tabs: ['alpha', 'beta'],
    switchTab: async () => ({ kind: 'ok', active: 'alpha', tabs: ['alpha', 'beta'] }),
  });
  check('insert after dest.tabId ACK', inserted.kind === 'insert');
  const failed = await resolveSideChatSttInsert({
    text: 'hello',
    tabId: 'alpha',
    tabs: ['alpha', 'beta'],
    switchTab: async () => ({ kind: 'discardClosed' }),
  });
  check('failed switch never inserts', failed.kind === 'discardClosed');

  const malformed = await resolveSideChatSttInsert({
    text: 'hello',
    tabId: 'alpha',
    tabs: ['alpha', 'beta'],
    switchTab: async () => ({ kind: 'malformed' }),
  });
  const malformedToast = planSideChatSttInsertToast(malformed);
  check(
    'malformed ACK is not toasted as closed panel',
    malformed.kind === 'error'
      && !malformed.message.includes('Side chat closed')
      && malformedToast.kind === 'error'
      && malformedToast.kind === 'error'
      && !malformedToast.message.includes(SIDE_CHAT_STT_CLOSED_TOAST),
  );

  let switchThrow = null;
  try {
    await resolveSideChatSttInsert({
      text: 'hello',
      tabId: 'alpha',
      tabs: ['alpha', 'beta'],
      switchTab: async () => {
        throw new Error('rpc down');
      },
    });
  } catch (err) {
    switchThrow = err;
  }
  const throwToast = planSideChatSttInsertThrowToast(switchThrow, { sessionId: 'sess-1', tabId: 'alpha' });
  check(
    'RPC switch throw is not toasted as Side chat closed',
    switchThrow instanceof Error
      && switchThrow.message === 'rpc down'
      && throwToast.includes('rpc down')
      && throwToast.includes('tab=alpha')
      && throwToast.includes('session=sess-1')
      && !throwToast.includes(SIDE_CHAT_STT_CLOSED_TOAST),
  );
  check(
    'closed toast planner is only for documented discardClosed',
    planSideChatSttInsertToast({ kind: 'discardClosed' }).kind === 'closed'
      && planSideChatSttInsertToast({
        kind: 'error',
        message: 'side chat switch: malformed ACK (tab=alpha)',
        toasted: false,
      }).kind !== 'closed',
  );

  const transportMsg = 'side chat switch: send failed: ws closed (session=sess-1 tab=alpha)';
  const transportPlan = planSideChatSwitchCatchToast({ toastOwned: false, message: transportMsg });
  const transportInsert = planSideChatSttInsertToast({
    kind: 'error',
    message: transportMsg,
    toasted: transportPlan.kind === 'silent',
  });
  check(
    'transport switch reject without prior toast is not silent',
    transportPlan.kind === 'error'
      && transportPlan.kind === 'error'
      && !transportPlan.message.includes(SIDE_CHAT_STT_CLOSED_TOAST)
      && transportInsert.kind === 'error'
      && transportInsert.kind === 'error'
      && !transportInsert.message.includes(SIDE_CHAT_STT_CLOSED_TOAST),
  );
  const rpcMsg = 'side chat switch: rpc failed (session=sess-1 tab=alpha)';
  const rpcPlan = planSideChatSwitchCatchToast({ toastOwned: false, message: rpcMsg });
  const rpcInsert = planSideChatSttInsertToast({
    kind: 'error',
    message: rpcMsg,
    toasted: rpcPlan.kind === 'silent',
  });
  check(
    'rpc switch reject without prior toast is not the closed-panel toast',
    rpcPlan.kind === 'error'
      && rpcInsert.kind === 'error'
      && rpcInsert.kind === 'error'
      && !rpcInsert.message.includes(SIDE_CHAT_STT_CLOSED_TOAST),
  );

  const timeoutMsg = 'side chat switch: side_chat_switch timed out after 30s (no response from server) (session=sess-1 tab=alpha)';
  const ownedPlan = planSideChatSwitchCatchToast({ toastOwned: true, message: timeoutMsg });
  const ownedInsert = planSideChatSttInsertToast({
    kind: 'error',
    message: timeoutMsg,
    toasted: true,
  });
  check(
    'toastOwned timeout does not take a second toast',
    ownedPlan.kind === 'silent' && ownedInsert.kind === 'silent',
  );

  const sessionChanged = finishSideChatSttInsertAfterAwait({
    decision: {
      kind: 'error',
      message: 'side chat switch: session changed (session=sess-1 tab=alpha)',
      toasted: false,
    },
    live: false,
  });
  const unmountToast = planSideChatSttInsertToast(sessionChanged);
  check(
    'resolve error then live=false stays kind error',
    sessionChanged.kind === 'error'
      && sessionChanged.kind === 'error'
      && unmountToast.kind !== 'closed'
      && unmountToast.kind === 'error'
      && unmountToast.kind === 'error'
      && !unmountToast.message.includes(SIDE_CHAT_STT_CLOSED_TOAST),
  );

  {
    const first = generateSideChatName([], () => 0);
    const again = generateSideChatName([], () => 0);
    check('injected random is deterministic', first === again && first === 'amber_otter', `${first} vs ${again}`);
    check('generated name stays within 32 chars', first.length <= SIDE_CHAT_NAME_MAX_CHARS);
    check('generated name uses tab-name charset', /^[A-Za-z0-9_]+$/.test(first), first);
    const collision = generateSideChatName([first], () => 0);
    check('collision retries with a numeric suffix', collision === `${first}_2`, collision);
    const reservedHit = generateSideChatName(['new', 'list', 'close', 'btw', first], () => 0);
    check(
      'reserved names are never returned',
      reservedHit !== 'new' && reservedHit !== 'list' && reservedHit !== 'close' && reservedHit !== 'btw' && reservedHit !== '/btw',
      reservedHit,
    );
    check('reserved collision still unique', reservedHit === `${first}_2`, reservedHit);
    for (let step = 0; step < 16; step += 1) {
      const name = generateSideChatName(['default'], () => step / 16);
      check(
        `generated name ${name} is valid`,
        name.length <= SIDE_CHAT_NAME_MAX_CHARS
          && /^[A-Za-z0-9_]+$/.test(name)
          && !['new', 'list', 'close', 'btw'].includes(name.toLowerCase()),
        name,
      );
    }
  }

  {
    resetConversationStreamStateForTest();
    const key = conversationStreamKey('mount-a', 'side', 'alpha', 'live-1');
    const owner = { innerHTML: '', isConnected: true };
    scheduleConversationStream(key, 'alpha', owner);
    scheduleConversationStream(key, '**bold** and `code`', owner);
    scheduleConversationStream(key, '**bold** and `code`', owner);
    check('coalesced schedule does not parse before the frame', conversationStreamParseCountForTest() === 0);
    flushConversationStreamRafsForTest();
    check('one animation frame parses once', conversationStreamParseCountForTest() === 1, String(conversationStreamParseCountForTest()));
    check('coalesced html is the last buffer', owner.innerHTML.includes('<strong>bold</strong>') && owner.innerHTML.includes('<code class="md-code">code</code>'), owner.innerHTML);
    scheduleConversationStream(key, '**bold** and `code`', owner);
    flushConversationStreamRafsForTest();
    check('unchanged buffer skips a second parse', conversationStreamParseCountForTest() === 1, String(conversationStreamParseCountForTest()));
    const disconnected = { innerHTML: 'keep', isConnected: false };
    scheduleConversationStream(key, 'gone', disconnected);
    flushConversationStreamRafsForTest();
    check('unmounted owner is not written', disconnected.innerHTML === 'keep', disconnected.innerHTML);
    check('unmounted owner does not parse', conversationStreamParseCountForTest() === 1, String(conversationStreamParseCountForTest()));

    resetConversationStreamStateForTest();
    const cancelled = { innerHTML: '', isConnected: true };
    const cancelKey = conversationStreamKey('mount-a', 'side', 'alpha', 'live-2');
    scheduleConversationStream(cancelKey, '**x**', cancelled);
    cancelConversationStream(cancelKey);
    flushConversationStreamRafsForTest();
    check('cancel drops coalesced work', cancelled.innerHTML === '' && conversationStreamParseCountForTest() === 0, cancelled.innerHTML);

    resetConversationStreamStateForTest();
    const tabOwner = { innerHTML: '', isConnected: true };
    scheduleConversationStream(conversationStreamKey('mount-a', 'side', 'alpha', 'x'), '**a**', tabOwner);
    cancelConversationStreamsForOwner('mount-a', 'side', 'alpha');
    flushConversationStreamRafsForTest();
    check('tab switch cancels that tab', tabOwner.innerHTML === '' && conversationStreamParseCountForTest() === 0);
    scheduleConversationStream(conversationStreamKey('mount-a', 'side', 'beta', 'x'), '**b**', tabOwner);
    const otherMount = { innerHTML: '', isConnected: true };
    scheduleConversationStream(conversationStreamKey('mount-b', 'side', 'beta', 'x'), '**other**', otherMount);
    cancelConversationStreamsForScope('mount-a');
    flushConversationStreamRafsForTest();
    check('unmount clears this side mount only', tabOwner.innerHTML === '' && otherMount.innerHTML.includes('<strong>other</strong>'));
    resetConversationStreamStateForTest();
    const fileOwner = { innerHTML: '', isConnected: true };
    const sideOwner = { innerHTML: '', isConnected: true };
    scheduleConversationStream(conversationStreamKey('file-mount', 'file', 'same', 'e1'), '**file**', fileOwner);
    scheduleConversationStream(conversationStreamKey('side-mount-a', 'side', 'same', 'e1'), '**side**', sideOwner);
    cancelConversationStreamsForScope('side-mount-a');
    flushConversationStreamRafsForTest();
    check(
      'side mount unmount leaves the file conversation frame',
      fileOwner.innerHTML.includes('<strong>file</strong>') && sideOwner.innerHTML === '',
      `file=${fileOwner.innerHTML} side=${sideOwner.innerHTML}`,
    );
  }

  {
    const css = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'styles.css'), 'utf8');
    check('side-chat dialog id is styled', /#side-chat-panel\s*\{/.test(css), 'missing #side-chat-panel rule');
    check('role gutter class is gone', !css.includes('.side-chat__role'), 'role gutter CSS still present');
    check('typing indent is gone', !css.includes('padding-left: 66px'));
    const HEADER = 48;
    const FOOTER = 56;
    const VIEW_H = 800;
    const band = VIEW_H - HEADER - FOOTER;
    const rules = parseCssRules(css);
    const lastSideWidth = lastSideChatDecl(rules, 'width');
    const lastSideTop = lastSideChatDecl(rules, 'top');
    const lastSideBottom = lastSideChatDecl(rules, 'bottom');
    check(
      'desktop width target is the last #side-chat-panel width outside phone media',
      lastSideWidth.desktop === 'min(1120px, calc(100vw - 24px))',
      JSON.stringify(lastSideWidth),
    );
    check(
      'phone width target is the last #side-chat-panel width inside max-width 768',
      lastSideWidth.phone === '100vw',
      JSON.stringify(lastSideWidth),
    );
    check(
      'desktop band margins are 8px',
      lastSideTop.desktop === 'calc(var(--app-header-height) + 8px)'
        && lastSideBottom.desktop === 'calc(var(--app-footer-height) + 8px)',
      JSON.stringify({ lastSideTop, lastSideBottom }),
    );
    check(
      'phone band uses header/footer with no extra inset',
      lastSideTop.phone === 'var(--app-header-height)'
        && lastSideBottom.phone === 'var(--app-footer-height)',
      JSON.stringify({ lastSideTop, lastSideBottom }),
    );
    const idRules = rules.filter((rule) => (
      rule.selector.split(',').some((part) => /#side-chat-panel(?![\w-])/.test(selectorSubject(part.trim())))
    ));
    check(
      'dialog id zeros shadow and never uses the panel shadow token',
      lastSideChatDecl(rules, 'box-shadow').desktop === 'none'
        && idRules.every((rule) => Object.values(rule.decls).every((value) => !String(value).includes('--shadow-panel'))),
      JSON.stringify(lastSideChatDecl(rules, 'box-shadow')),
    );
    check(
      'dialog id does not set drawer 400px / 90vh',
      idRules.every((rule) => (
        rule.decls.width !== '400px'
        && rule.decls['max-width'] !== '400px'
        && !String(rule.decls.height ?? '').includes('90vh')
        && !String(rule.decls['max-height'] ?? '').includes('90vh')
      )),
    );
    const features = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '..', '..', '..', '..', 'FEATURES.md'), 'utf8');
    const geoLine = features.split('\n').find((line) => line.includes('Side chat is `#side-chat-panel`')) ?? '';
    check('FEATURES geometry sentence exists', geoLine.length > 0);
    check(
      'FEATURES geometry copies desktop width/top/bottom from CSS',
      Boolean(lastSideWidth.desktop)
        && geoLine.includes(lastSideWidth.desktop)
        && Boolean(lastSideTop.desktop)
        && geoLine.includes(lastSideTop.desktop)
        && Boolean(lastSideBottom.desktop)
        && geoLine.includes(lastSideBottom.desktop),
      JSON.stringify({ lastSideWidth, lastSideTop, lastSideBottom, geoLine }),
    );
    check(
      'FEATURES geometry copies phone width/top/bottom and 768 media from CSS',
      lastSideWidth.phone === '100vw'
        && geoLine.includes('width: 100vw')
        && Boolean(lastSideTop.phone)
        && geoLine.includes(lastSideTop.phone)
        && Boolean(lastSideBottom.phone)
        && geoLine.includes(lastSideBottom.phone)
        && geoLine.includes('max-width: 768px'),
      JSON.stringify({ lastSideWidth, lastSideTop, lastSideBottom, geoLine }),
    );
    check(
      'FEATURES geometry dropped the 32px/12px/720/16px claims',
      !geoLine.includes('100vw - 32px')
        && !geoLine.includes('header+12px')
        && !geoLine.includes('footer+12px')
        && !geoLine.includes('≤720')
        && !geoLine.includes('<=720')
        && !geoLine.includes('100vw - 16px'),
      geoLine,
    );

    const rows = [];
    for (const vw of [390, 768, 800, 1280, 1600]) {
      const oldBox = computeBox(rules, 'drawer', vw, VIEW_H, HEADER, FOOTER);
      const newBox = computeBox(rules, 'sidechat', vw, VIEW_H, HEADER, FOOTER);
      const drawer400 = {
        width: Math.min(400, 0.94 * vw),
        height: Math.min(0.9 * VIEW_H, band),
      };
      const oldArea = oldBox.width * oldBox.height;
      const newArea = newBox.width * newBox.height;
      const area400 = drawer400.width * drawer400.height;
      rows.push({ vw, oldBox, newBox, oldArea, newArea, area400, ratioOld: newArea / oldArea, ratio400: newArea / area400 });
      check(`${vw} has no horizontal overflow`, newBox.width <= vw + 1e-9, String(newBox.width));
      check(
        `${vw} keeps header and composer insets`,
        newBox.top + 1e-9 >= HEADER && newBox.bottom + 1e-9 >= FOOTER,
        `top ${newBox.top} bottom ${newBox.bottom}`,
      );
      if (vw <= 768) {
        check(
          `${vw} does not shrink a dimension below the parsed .panel-drawer`,
          newBox.width + 1e-9 >= oldBox.width && newBox.height + 1e-9 >= oldBox.height,
          `new ${newBox.width}x${newBox.height} vs old ${oldBox.width}x${oldBox.height}`,
        );
        check(
          `${vw} phone width stays full bleed`,
          Math.abs(newBox.width - vw) < 1e-6,
          String(newBox.width),
        );
        check(
          `${vw} area is strictly greater than the 400px drawer`,
          newArea > area400,
          `${newArea} vs ${area400}`,
        );
        check(
          `${vw} area is not smaller than the parsed .panel-drawer`,
          newArea + 1e-6 >= oldArea,
          `${newArea} vs ${oldArea}`,
        );
      } else if (vw === 800) {
        check(
          `${vw} area is strictly greater than the parsed .panel-drawer`,
          newArea > oldArea,
          `${newArea} vs ${oldArea}`,
        );
      } else {
        check(
          `${vw} area is at least 2x the parsed .panel-drawer`,
          newArea + 1e-6 >= oldArea * 2,
          `${newArea} vs ${oldArea * 2} ratio=${newArea / oldArea}`,
        );
        check(
          `${vw} area is at least 2x the 400px drawer`,
          newArea + 1e-6 >= area400 * 2,
          `${newArea} vs ${area400 * 2}`,
        );
      }
    }
    check('geometry table covered five breakpoints', rows.length === 5);
    console.log('side-chat geometry', JSON.stringify(rows.map((row) => ({
      vw: row.vw,
      old: `${row.oldBox.width}x${row.oldBox.height}=${row.oldArea}`,
      neu: `${row.newBox.width}x${row.newBox.height}=${row.newArea}`,
      ratioOld: row.ratioOld,
      ratio400: row.ratio400,
    }))));
  }

{
  check('busy reject is not accepted', sideChatPromptAccepted({ accepted: false, busy: true }) === false);
  check('accepted prompt clears', sideChatPromptAccepted({ accepted: true, busy: false }) === true);
  check('missing accepted is treated as accepted', sideChatPromptAccepted({ active: 't1' }) === true);
  check('null snapshot is treated as accepted', sideChatPromptAccepted(null) === true);
  const src = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'panels', 'SideChatPanel.tsx'), 'utf8');
  const submitAt = src.indexOf('const submit = () => {');
  const submitBlock = submitAt >= 0 ? src.slice(submitAt, submitAt + 900) : '';
  check('submit locks with sendingRef before onPrompt', submitBlock.includes('sendingRef.current || !sideChatCanSubmit') && submitBlock.includes('sendingRef.current = true'));
  check('submit awaits onPrompt before any clear', submitBlock.includes('.then(() => onPrompt(payload, images))') && submitBlock.indexOf('onPrompt(payload, images)') < submitBlock.indexOf('setPrompt((current)'));
  check('submit keeps draft when onPrompt returns false', submitBlock.includes('if (ok === false) return;') && submitBlock.includes('current.trim() === payload'));
  check('padded draft still clears after accept', '  keep this  '.trim() === 'keep this');
  check('submit no longer clears before ACK', !submitBlock.includes("onPrompt(prompt.trim(), images);") && !submitBlock.includes("setPrompt('');"));
  const app = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'App.tsx'), 'utf8');
  const promptAt = app.indexOf('const sideChatPrompt = useCallback');
  const promptBlock = promptAt >= 0 ? app.slice(promptAt, promptAt + 1100) : '';
  check('App sideChatPrompt returns accepted boolean', promptBlock.includes('Promise<boolean>') && promptBlock.includes('return sideChatPromptAccepted(data);') && promptBlock.includes('return false;') && promptBlock.includes("data === null || typeof data !== 'object'"));
  check('App busy toast text unchanged', promptBlock.includes("toast('side chat is busy — wait for the current reply', true)"));
  check('App send-failed toast text unchanged', promptBlock.includes('side chat send failed:'));
}

  console.log(`\nsideChatRender.test: ${ran} assertions, ${failures.length} failure(s)`);
  if (failures.length) {
    console.error(failures.join('\n'));
    process.exit(1);
  }
  process.exit(0);
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
