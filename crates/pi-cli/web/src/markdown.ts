import { escapeHtml, escapeHtmlRaw, redactSecrets, safeImage } from './redact';
import katex from 'katex';
import mermaid from 'mermaid';
import { codeLanguage } from './attachments';
// highlight.js core build + a focused language subset (tree-shakeable
// per-language imports; auto-detection uses only the registered set).
import hljs from 'highlight.js/lib/core';
import bash from 'highlight.js/lib/languages/bash';
import c from 'highlight.js/lib/languages/c';
import cpp from 'highlight.js/lib/languages/cpp';
import css from 'highlight.js/lib/languages/css';
import diff from 'highlight.js/lib/languages/diff';
import go from 'highlight.js/lib/languages/go';
import java from 'highlight.js/lib/languages/java';
import javascript from 'highlight.js/lib/languages/javascript';
import json from 'highlight.js/lib/languages/json';
import markdown from 'highlight.js/lib/languages/markdown';
import plaintext from 'highlight.js/lib/languages/plaintext';
import python from 'highlight.js/lib/languages/python';
import rust from 'highlight.js/lib/languages/rust';
import sql from 'highlight.js/lib/languages/sql';
import typescript from 'highlight.js/lib/languages/typescript';
import xml from 'highlight.js/lib/languages/xml';
import yaml from 'highlight.js/lib/languages/yaml';
import 'highlight.js/styles/github-dark.css';
import 'katex/dist/katex.min.css';
import { normalizeThinkingNewlines } from './transcript';
import type { ContentBlock } from './types';

const hljsLanguages = {
  bash, c, cpp, css, diff, go, java, javascript, json, markdown,
  plaintext, python, rust, sql, typescript, xml, yaml,
};
for (const [name, def] of Object.entries(hljsLanguages)) {
  hljs.registerLanguage(name, def);
}

/* ------------------------------------------------------------------ *
 * Diff-line highlighting (code-review pane). One hljs call per contiguous
 * run of lines instead of one per line; each returned fragment is balanced
 * and its textContent equals redactSecrets(line.text) — the exact text the
 * plain renderer (safeText) shows, credential-shaped content included.
 * ------------------------------------------------------------------ */

/**
 * Resolve a registered highlight.js language for a repo-relative path via the
 * shared filename→language mapping (attachments.ts). Returns null when the
 * path maps to no language or to a language outside the registered subset —
 * such lines render as plain text.
 */
export function diffPathLanguage(path: string): string | null {
  const hint = codeLanguage(path);
  if (!hint) return null;
  const base = hint.toLowerCase();
  return hljs.getLanguage(base) ? base : null;
}

/**
 * Split one hljs block output into per-line fragments. hljs emits literal
 * '\n' for newlines and a token may span lines (multi-line comments, raw
 * strings); at every line boundary the open spans are closed and re-opened on
 * the next line so each fragment is balanced HTML with the same textContent
 * as the joined source. The tag walk only matches hljs's own
 * `<span class="...">` / `</span>` pairs.
 */
function splitHighlightedLines(html: string): string[] {
  const lines: string[] = [];
  const openTags: string[] = [];
  let current = '';
  let index = 0;
  while (index < html.length) {
    const ch = html[index];
    if (ch === '<') {
      const tagEnd = html.indexOf('>', index);
      if (tagEnd === -1) {
        current += html.slice(index);
        break;
      }
      const tag = html.slice(index, tagEnd + 1);
      if (tag.startsWith('<span class="')) {
        openTags.push(tag);
        current += tag;
      } else if (tag === '</span>') {
        openTags.pop();
        current += tag;
      } else {
        current += tag;
      }
      index = tagEnd + 1;
      continue;
    }
    if (ch === '\n') {
      if (openTags.length > 0) {
        current += '</span>'.repeat(openTags.length);
      }
      lines.push(current);
      current = openTags.length > 0 ? openTags.join('') : '';
      index += 1;
      continue;
    }
    current += ch;
    index += 1;
  }
  lines.push(current);
  return lines;
}

/**
 * Batch-highlight diff line bodies. `texts` uses null for lines that must
 * stay plain (meta lines); each maximal run of non-null lines is highlighted
 * with ONE hljs call (bounded hunk/page granularity) and split back into one
 * balanced HTML fragment per line. `language` must be a registered language
 * name (see diffPathLanguage). Fragments are hljs-escaped — hostile input
 * stays literal — and every line passes through redactSecrets() FIRST so the
 * highlighted textContent is byte-identical to what the pane's plain renderer
 * (safeText) shows: ordinary/hostile source verbatim, credential-shaped
 * content as [REDACTED]. A thrown grammar (or a fragment-count mismatch)
 * falls back to plain escaped text per line with the same redaction.
 */
export function highlightDiffLineFragments(
  language: string,
  texts: ReadonlyArray<string | null>,
): Array<string | null> {
  const fragments: Array<string | null> = new Array(texts.length).fill(null);
  let runStart = -1;
  const flushRun = (end: number) => {
    if (runStart < 0 || end <= runStart) return;
    const run = texts.slice(runStart, end) as string[];
    const highlighted = highlightLineRun(language, run);
    for (let i = 0; i < highlighted.length; i++) {
      fragments[runStart + i] = highlighted[i];
    }
    runStart = -1;
  };
  for (let i = 0; i <= texts.length; i++) {
    if (i < texts.length && texts[i] !== null) {
      if (runStart < 0) runStart = i;
    } else {
      flushRun(i);
    }
  }
  return fragments;
}

function highlightLineRun(language: string, texts: string[]): string[] {
  // Redact FIRST, exactly like the pane's plain renderer (safeText): the
  // textContent contract is identical for highlighted and plain lines, so
  // credential-shaped content becomes [REDACTED] in both while ordinary or
  // hostile source stays verbatim.
  const redacted = texts.map((text) => redactSecrets(text));
  try {
    const joined = hljs.highlight(redacted.join('\n'), {
      language,
      ignoreIllegals: true,
    }).value;
    const fragments = splitHighlightedLines(joined);
    if (fragments.length === redacted.length) return fragments;
    return redacted.map((text) => escapeHtmlRaw(text));
  } catch {
    return redacted.map((text) => escapeHtmlRaw(text));
  }
}

// Markdown renderer for assistant text. The input is RAW model text; the
// pipeline guarantees no model characters reach the HTML surface unescaped:
//
//   1. extractFences — pull ```lang ... ``` blocks out of the raw text first,
//      so fence bodies are never interpreted as markdown and never mangled
//      by escaping before they reach the fence renderer
//   2. extractMath   — pull $...$ / $$...$$ segments out of the remaining
//      text; they become control-character placeholders that survive
//      escapeHtml and are swapped for KaTeX HTML afterwards
//   3. escapeHtml    — every remaining model character is escaped
//   4. renderLines   — block transforms on the escaped text (headings,
//      tables, task lists, nested lists, blockquotes, hr, paragraphs)
//   5. inlineMd      — inline transforms on already-escaped text
//   6. substituteMath — placeholder -> KaTeX output (KaTeX escapes its input)
//
// Mermaid fences render as `.md-mermaid-host` divs whose textContent is the
// diagram source; hydrateMermaid() replaces them with SVG asynchronously
// using securityLevel: 'strict' (mermaid's XSS sanitizer — never disabled).
// Failed diagrams degrade to a styled block showing the raw source: they are
// never eval'd and errors never escape the render path.

/* ------------------------------------------------------------------ *
 * Math extraction ($...$ inline, $$...$$ display)
 * ------------------------------------------------------------------ */

interface MathPart {
  tex: string;
  display: boolean;
}

// Scanner that lifts math out of raw text while leaving inline code spans
// (backtick runs) alone. Output text uses \u0001M<n>\u0002 placeholders,
// which escapeHtml cannot alter.
function extractMath(raw: string): { text: string; parts: MathPart[] } {
  let out = '';
  const parts: MathPart[] = [];
  let i = 0;
  while (i < raw.length) {
    const ch = raw[i];
    if (ch === '`') {
      const span = raw.slice(i).match(/^(`+)([\s\S]*?)\1/);
      if (span) {
        out += span[0];
        i += span[0].length;
        continue;
      }
      out += ch;
      i += 1;
      continue;
    }
    if (ch === '$') {
      const prev = i > 0 ? raw[i - 1] : '';
      if (/\d/.test(prev)) {
        // currency amount, not math
        out += ch;
        i += 1;
        continue;
      }
      const rest = raw.slice(i);
      const display = rest.match(/^\$\$([\s\S]*?)\$\$/);
      if (display && display[1].trim() !== '') {
        parts.push({ tex: display[1], display: true });
        out += `\u0001M${parts.length - 1}\u0002`;
        i += display[0].length;
        continue;
      }
      const inline = rest.match(/^\$(\S(?:[^$\n]*?\S)?)\$/);
      if (inline) {
        parts.push({ tex: inline[1], display: false });
        out += `\u0001M${parts.length - 1}\u0002`;
        i += inline[0].length;
        continue;
      }
    }
    out += ch;
    i += 1;
  }
  return { text: out, parts };
}

function mathHtml(tex: string, display: boolean): string {
  try {
    // KaTeX escapes its input; redactSecrets keeps credential-shaped math out.
    return katex.renderToString(redactSecrets(tex), {
      displayMode: display,
      throwOnError: false,
      strict: false,
      trust: false,
    });
  } catch {
    return escapeHtml(tex); // malformed math falls back to an escaped literal
  }
}

function substituteMath(html: string, parts: MathPart[]): string {
  if (parts.length === 0) return html;
  return html.replace(/\u0001M(\d+)\u0002/g, (_m, index: string) => {
    const part = parts[Number(index)];
    return part ? mathHtml(part.tex, part.display) : '';
  });
}

/* ------------------------------------------------------------------ *
 * Fence extraction (```lang ... ```) — runs before escaping
 * ------------------------------------------------------------------ */

type Segment =
  | { kind: 'text'; text: string }
  | { kind: 'fence'; lang: string; source: string };

function extractFences(raw: string): Segment[] {
  const segments: Segment[] = [];
  const lines = raw.split('\n');
  let buf: string[] = [];
  let inFence = false;
  let lang = '';
  const flushText = () => {
    if (buf.length > 0) {
      segments.push({ kind: 'text', text: buf.join('\n') });
      buf = [];
    }
  };
  for (const line of lines) {
    const fence = line.match(/^```(\S*)\s*$/);
    if (fence) {
      if (inFence) {
        segments.push({ kind: 'fence', lang, source: buf.join('\n') });
        buf = [];
        inFence = false;
      } else {
        flushText();
        inFence = true;
        lang = fence[1] || '';
      }
      continue;
    }
    buf.push(line);
  }
  if (inFence) {
    // Unclosed fence: still render as a fence block.
    segments.push({ kind: 'fence', lang, source: buf.join('\n') });
  } else {
    flushText();
  }
  return segments;
}

export function renderFence(lang: string, source: string): string {
  // Normalize the fence info string. Markdown code fences may carry
  // comma-separated metadata after the language token (```rust,ignore,
  // ```rs,no_run). Without stripping the suffix, hljs.getLanguage("rust,ignore")
  // returns undefined and the renderer falls back to highlightAuto, which
  // uses ignoreIllegals:false internally. The Rust grammar declares
  // `illegal: '</` and bails on many real-world snippets, so a different
  // language (or plaintext) with only a few overlapping keyword spans wins
  // the relevance race — most of the block renders as plain text, i.e.
  // "only a small part is highlighted."
  //
  // Splitting to the base token lets getLanguage resolve registered aliases
  // (the rust grammar registers `rs` as an alias), and ignoreIllegals:true
  // forces the grammar to consume the ENTIRE source, emitting every token —
  // including late ones — as hljs spans. The catch falls back to plain
  // escaping so a fence can never break rendering. hljs output is already
  // HTML-escaped; the copy button reads textContent, which stays the source.
  const baseLang = lang.split(/[,;\s]/)[0].toLowerCase();
  if (baseLang === 'mermaid') {
    // Host for async hydration: textContent is the diagram source (escaped
    // here, decoded back by the browser), replaced by SVG in hydrateMermaid().
    return `<div class="md-mermaid-host">${escapeHtml(source)}</div>`;
  }
  const registered = baseLang !== '' ? hljs.getLanguage(baseLang) : undefined;
  let highlighted: string;
  try {
    highlighted = registered
      ? hljs.highlight(source, { language: baseLang, ignoreIllegals: true }).value
      : hljs.highlightAuto(source).value;
  } catch {
    highlighted = escapeHtml(source);
  }
  const label = registered
    ? (registered.name || baseLang).toLowerCase()
    : (baseLang !== '' ? baseLang : 'text');
  return (
    `<div class="md-fence">` +
    `<div class="md-fence__head"><span class="md-fence__lang">${escapeHtml(label)}</span>` +
    `<button type="button" class="md-fence__copy">copy</button></div>` +
    `<pre class="md-fence__pre"><code class="md-code hljs">${highlighted}</code></pre>` +
    `</div>`
  );
}

/* ------------------------------------------------------------------ *
 * URL / image policy
 * ------------------------------------------------------------------ */

function safeUrl(raw: string): string {
  const url = raw.trim();
  if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(url)) {
    return /^(https?|mailto):/i.test(url) ? url : '';
  }
  if (url === '' || url === '#') return url;
  if (url.charAt(0) === '/' || url.indexOf('./') === 0 || url.indexOf('../') === 0) {
    return url;
  }
  return '';
}

// Image sources: whitelisted-MIME base64 data URIs, http(s), or same-origin
// relative paths. Nothing else (no javascript:, no mailto, no other schemes).
function imageSrc(raw: string): string {
  const url = raw.trim();
  const data = url.match(/^data:image\/(png|jpeg|gif|webp);base64,([A-Za-z0-9+/=\s]+)$/);
  if (data) return `data:image/${data[1]};base64,${data[2]}`;
  const safe = safeUrl(url);
  if (!safe) return '';
  if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(safe)) {
    return /^https?:/i.test(safe) ? safe : '';
  }
  return safe;
}

/* ------------------------------------------------------------------ *
 * Inline transforms (operates on already-escaped text)
 * ------------------------------------------------------------------ */

function inlineMd(escaped: string): string {
  let s = escaped;
  s = s.replace(/`([^`]+)`/g, (_m, t: string) => `<code class="md-code">${t}</code>`);
  // Images before links: ![alt](url) must not be consumed by the link rule.
  s = s.replace(/!\[([^\]]*)\]\(([^)]+)\)/g, (m, alt: string, url: string) => {
    const src = imageSrc(url);
    return src
      ? `<img class="md-image" src="${src}" alt="${alt}" loading="lazy" rel="noopener noreferrer">`
      : m; // invalid URL: keep the original (escaped) markdown literal
  });
  s = s.replace(/\[([^\]]+)\]\(([^)]+)\)/g, (_m, label: string, url: string) => {
    const target = safeUrl(url);
    return target ? `<a href="${target}" rel="noopener noreferrer">${label}</a>` : label;
  });
  s = s.replace(/\*\*([^*]+)\*\*/g, (_m, t: string) => `<strong>${t}</strong>`);
  s = s.replace(/(^|[^*\n])\*([^*\n]+)\*(?!\*)/g, (_m, pre: string, t: string) => `${pre}<em>${t}</em>`);
  return s;
}

/* ------------------------------------------------------------------ *
 * Block transforms (operates on already-escaped text)
 * ------------------------------------------------------------------ */

function splitCells(line: string): string[] {
  const cells = line.split('|').map((c) => c.trim());
  if (cells.length > 0 && cells[0] === '') cells.shift();
  if (cells.length > 0 && cells[cells.length - 1] === '') cells.pop();
  return cells;
}

function isTableSeparator(line: string): boolean {
  if (!line.includes('|')) return false;
  const cells = splitCells(line);
  return cells.length > 0 && cells.every((c) => /^:?-+:?$/.test(c));
}

function tryTable(lines: string[], i: number): { html: string; consumed: number } | null {
  if (!lines[i].includes('|')) return null;
  if (i + 1 >= lines.length || !isTableSeparator(lines[i + 1])) return null;
  const header = splitCells(lines[i]);
  const body: string[][] = [];
  let j = i + 2;
  while (j < lines.length && lines[j].trim() !== '' && lines[j].includes('|')) {
    body.push(splitCells(lines[j]));
    j++;
  }
  const headRow = `<tr>${header.map((c) => `<th scope="col">${inlineMd(c)}</th>`).join('')}</tr>`;
  const bodyRows = body
    .map((row) => `<tr>${row.map((c) => `<td>${inlineMd(c)}</td>`).join('')}</tr>`)
    .join('');
  return {
    html: `<table class="md-table"><thead>${headRow}</thead>${bodyRows ? `<tbody>${bodyRows}</tbody>` : ''}</table>`,
    consumed: j - i,
  };
}

interface ListLevel {
  tag: 'ul' | 'ol';
  indent: number;
}

function renderLines(escaped: string): string {
  const lines = escaped.split('\n');
  let html = '';
  const stack: ListLevel[] = [];

  const closeAllLists = () => {
    while (stack.length > 0) {
      // Every open level always has exactly one unclosed <li>.
      html += `</li></${stack.pop()!.tag}>`;
    }
  };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];

    const table = tryTable(lines, i);
    if (table) {
      closeAllLists();
      html += table.html;
      i += table.consumed - 1;
      continue;
    }

    const heading = line.match(/^(#{1,6})\s+(.*)$/);
    if (heading) {
      closeAllLists();
      const level = heading[1].length;
      html += `<h${level} class="md-h${level}">${inlineMd(heading[2])}</h${level}>`;
      continue;
    }

    if (/^\s*(---+|\*\*\*+)\s*$/.test(line)) {
      closeAllLists();
      html += '<hr>';
      continue;
    }

    if (/^&gt;\s?/.test(line)) {
      closeAllLists();
      const quote: string[] = [];
      while (i < lines.length && /^&gt;\s?/.test(lines[i])) {
        quote.push(inlineMd(lines[i].replace(/^&gt;\s?/, '')));
        i++;
      }
      i--;
      html += `<blockquote class="md-quote">${quote.join('<br>')}</blockquote>`;
      continue;
    }

    const item = line.match(/^(\s*)([-*+]|\d+\.)\s+(\[([ xX])\]\s+)?(.*)$/);
    if (item) {
      const indent = item[1].length;
      const tag: 'ul' | 'ol' = /^\d/.test(item[2]) ? 'ol' : 'ul';
      let content = item[5];
      const glyph = item[4];
      if (glyph !== undefined) {
        content = `<span class="md-task-glyph">${glyph === 'x' || glyph === 'X' ? '☑' : '☐'}</span> ${content}`;
      }
      // Adjust the open-list stack to the item's indent. Deferred </li>
      // handling keeps nested lists inside their parent item.
      while (stack.length > 0 && stack[stack.length - 1].indent > indent) {
        html += `</li></${stack.pop()!.tag}>`;
      }
      if (stack.length === 0 || stack[stack.length - 1].indent < indent) {
        html += `<${tag} class="md-list">`;
        stack.push({ tag, indent });
      } else if (stack[stack.length - 1].tag !== tag) {
        html += `</li></${stack.pop()!.tag}>`;
        html += `<${tag} class="md-list">`;
        stack.push({ tag, indent });
      } else {
        html += '</li>';
      }
      // </li> is deferred: it is emitted when a sibling item starts or the
      // list level closes, so a nested list stays inside its parent item.
      html += `<li>${inlineMd(content)}`;
      continue;
    }

    closeAllLists();
    if (line.trim() !== '') {
      html += `<p class="md-p">${inlineMd(line)}</p>`;
    }
  }
  closeAllLists();
  return html;
}

/* ------------------------------------------------------------------ *
 * Public render API
 * ------------------------------------------------------------------ */

/** Render raw markdown (table, math, mermaid, task lists, fences, ...). */
export function renderMarkdown(raw: string): string {
  const segments = extractFences(raw);
  let html = '';
  for (const segment of segments) {
    if (segment.kind === 'fence') {
      html += renderFence(segment.lang, segment.source);
      continue;
    }
    const { text, parts } = extractMath(segment.text);
    const escaped = escapeHtml(text);
    html += substituteMath(renderLines(escaped), parts);
  }
  return html;
}

/** Shared thinking header (brain icon + `Thinking`) as React-free HTML —
 *  identical for the streaming summary (App.tsx StreamingAssistant) and the
 *  restored/final rendering, so the two paths can never drift. The icon is an
 *  inline SVG (no asset, no dependency); text is the literal title. */
export function thinkingSummaryHtml(): string {
  return '<svg class="thinking__icon" viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">'
    + '<path d="M9.5 2A2.5 2.5 0 0 1 12 4.5v15a2.5 2.5 0 0 1-4.96.44 2.5 2.5 0 0 1-2.96-3.08 3 3 0 0 1-.34-5.58 2.5 2.5 0 0 1 1.32-4.24 2.5 2.5 0 0 1 1.98-3A2.5 2.5 0 0 1 9.5 2Z"/>'
    + '<path d="M14.5 2A2.5 2.5 0 0 0 12 4.5v15a2.5 2.5 0 0 0 4.96.44 2.5 2.5 0 0 0 2.96-3.08 3 3 0 0 0 .34-5.58 2.5 2.5 0 0 0-1.32-4.24 2.5 2.5 0 0 0-1.98-3A2.5 2.5 0 0 0 14.5 2Z"/>'
    + '</svg>Thinking';
}

/** Render one assistant message's content blocks (final, non-streaming). */
export function renderBlocks(blocks: ContentBlock[]): string {
  let html = '';
  for (const block of blocks) {
    if (!block) continue;
    if (block.type === 'text' && typeof block.text === 'string' && block.text !== '') {
      html += renderMarkdown(block.text);
    } else if (block.type === 'thinking' && typeof block.thinking === 'string') {
      // Same conservative literal-`\n` normalization the live delta path
      // applies (transcript.ts), so restored thinking bodies render
      // multi-line exactly like the streaming body; markdown stays safe
      // because normalization runs BEFORE renderMarkdown.
      html += `<details class="thinking" open><summary class="thinking__summary">${thinkingSummaryHtml()}</summary><div class="thinking__body">${renderMarkdown(normalizeThinkingNewlines(block.thinking))}</div></details>`;
    } else if (block.type === 'image') {
      const src = safeImage(block.mimeType, block.data);
      if (src) html += `<img class="md-image" src="${src}" alt="image">`;
    }
    // toolCall blocks are rendered as execution cards by tool_execution_* events
  }
  return html;
}

/* ------------------------------------------------------------------ *
 * Mermaid hydration (async, post-mount)
 * ------------------------------------------------------------------ */

let mermaidInitialized = false;
let mermaidSeq = 0;

function initMermaid(): void {
  if (mermaidInitialized) return;
  mermaidInitialized = true;
  mermaid.initialize({
    startOnLoad: false,
    securityLevel: 'strict', // mermaid's XSS sanitizer — never disable
    theme: 'dark',
  });
}

/**
 * Replace `.md-mermaid-host` elements inside `root` with rendered SVG.
 * Never throws: parse failures (and initialization failures) degrade to a
 * styled block showing the raw source. Safe to call repeatedly.
 *
 * `onMutated` is invoked SYNCHRONOUSLY, immediately after each mutation that
 * replaces a host's content (rendered SVG, error block, or empty-host clear)
 * — a layout-relevant height change that arrives with no React commit and no
 * streamed delta. The transcript pin controller uses it to re-pin the
 * viewport in the SAME task as the mutation, so the next frame is already
 * glued to the bottom; a ResizeObserver alone delivers one frame later.
 * Never called for hosts skipped via `data-mermaid="done"`, and never called
 * when mermaid initialization fails because no mutation happened.
 */
export async function hydrateMermaid(root: HTMLElement, onMutated?: () => void): Promise<void> {
  try {
    initMermaid();
  } catch {
    return;
  }
  const hosts = Array.from(root.querySelectorAll('.md-mermaid-host'));
  for (const host of hosts) {
    if (host.getAttribute('data-mermaid') === 'done') continue;
    host.setAttribute('data-mermaid', 'done'); // claim before the async render
    const source = host.textContent ?? '';
    if (source.trim() === '') {
      host.innerHTML = '';
      onMutated?.();
      continue;
    }
    try {
      const { svg } = await mermaid.render(`mmd-${mermaidSeq++}`, source);
      host.innerHTML = svg;
      host.classList.add('md-mermaid-host--rendered');
    } catch {
      host.innerHTML =
        '<div class="md-mermaid-error">' +
        '<span class="md-mermaid-error__note">mermaid: could not render this diagram</span>' +
        `<pre class="md-mermaid-error__source">${escapeHtml(source)}</pre>` +
        '</div>';
      host.classList.add('md-mermaid-host--error');
    }
    onMutated?.();
  }
}

/* ------------------------------------------------------------------ *
 * Code-fence copy button (delegated, fires once at module load)
 * ------------------------------------------------------------------ */

if (typeof document !== 'undefined') {
  document.addEventListener('click', (event) => {
    const target = event.target as HTMLElement | null;
    const button = target?.closest?.('.md-fence__copy') as HTMLButtonElement | null;
    if (!button) return;
    const code = button.closest('.md-fence')?.querySelector('code');
    if (!code || code.textContent === null) return;
    const text = code.textContent;
    const flash = () => {
      const original = button.textContent;
      button.textContent = 'copied';
      window.setTimeout(() => {
        button.textContent = original;
      }, 1200);
    };
    if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
      navigator.clipboard.writeText(text).then(flash, flash);
    } else {
      const textarea = document.createElement('textarea');
      textarea.value = text;
      textarea.style.position = 'fixed';
      textarea.style.opacity = '0';
      document.body.appendChild(textarea);
      textarea.select();
      try {
        document.execCommand('copy');
      } catch {
        /* clipboard unavailable — nothing else to do */
      }
      document.body.removeChild(textarea);
      flash();
    }
  });
}
