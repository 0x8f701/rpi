import { escapeHtml, escapeHtmlRaw, redactSecrets, safeImage } from './redact';
import katex from 'katex';
import mermaid from 'mermaid';
import { codeLanguage } from './attachments';
import { MERMAID_AUTHOR_CANVAS_FILL, MERMAID_HOST_TOOLBAR_HTML, sanitizeMermaidSvg } from './mermaidViewer';
import {
  configuredPlantumlServer,
  configurePlantumlServer,
  encodePlantumlSource,
  plantumlDocumentSource,
  plantumlHttpErrorDetail,
  plantumlSvgRequestUrl,
  subscribePlantumlServer,
} from './plantumlEncode';
import {
  fileExplorerCanonicalPath,
  fileExplorerCommandForSession,
  fileExplorerReadCommand,
  fileExplorerResolvedRequestPath,
  normalizeRead,
} from './fileExplorer';

// highlight.js core build + a focused language subset (tree-shakeable
// per-language imports; auto-detection uses only the registered set).
import hljs from 'highlight.js/lib/core';
import bash from 'highlight.js/lib/languages/bash';
import c from 'highlight.js/lib/languages/c';
import cpp from 'highlight.js/lib/languages/cpp';
import css from 'highlight.js/lib/languages/css';
import diff from 'highlight.js/lib/languages/diff';
import go from 'highlight.js/lib/languages/go';
import ini from 'highlight.js/lib/languages/ini';
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
import type { LanguageFn } from 'highlight.js';

/** Local, typed Solidity grammar — hljs ships none through 11.11.1, yet `.sol`
 *  sources render in the explorer preview and review diff: keyword groups, a
 *  bounded type-token regex, string/comment/number/pragma modes, definition
 *  name spans (shipped-grammar shape); disableAutodetect set. */
const solidity: LanguageFn = (hljs) => {
  const KEYWORDS = {
    keyword:
      'pragma import from as is using contract interface library abstract ' +
      'function modifier constructor event error struct enum mapping new ' +
      'delete emit return returns if else for while do break continue try ' +
      'catch revert require assert unchecked virtual override memory storage ' +
      'calldata constant immutable payable public private internal external ' +
      'pure view anonymous indexed receive fallback this super assembly let ' +
      'leave switch case default',
    literal: 'true false wei gwei ether seconds minutes hours days weeks',
    built_in:
      'msg block tx abi selfdestruct keccak256 sha256 ripemd160 ecrecover ' +
      'addmod mulmod gasleft blockhash type',
  };
  const doctag = { scope: 'doctag', begin: /@(?:title|author|notice|dev|param|return|inheritdoc|custom)\b/ };
  const comments = [
    hljs.COMMENT('//', '$', { contains: [doctag] }),
    hljs.COMMENT('/\\*', '\\*/', { contains: [doctag] }),
  ];
  const strings = [
    hljs.QUOTE_STRING_MODE,
    hljs.APOS_STRING_MODE,
    { scope: 'string', begin: /\b(?:hex|unicode)"/, end: /"/, illegal: /\n/, contains: [hljs.BACKSLASH_ESCAPE] },
  ];
  return {
    name: 'Solidity',
    aliases: ['sol'],
    disableAutodetect: true,
    keywords: KEYWORDS,
    contains: [
      ...comments,
      ...strings,
      { scope: 'meta', begin: /\bpragma\b/, end: /;/ },
      {
        scope: 'number',
        begin: /\b0[xX][a-fA-F0-9_]+\b|\b[0-9][0-9_]*(?:\.[0-9_]+)?(?:[eE][+-]?[0-9]+)?\b/,
      },
      {
        scope: 'type',
        begin: /\b(?:address|bool|string|byte|bytes[0-9]{0,2}|u?int[0-9]{0,3}|u?fixed[0-9]{0,4}(?:x[0-9]{1,4})?)\b/,
      },
      {
        scope: { 1: 'keyword', 3: 'title.function' },
        begin: [/\b(?:function|modifier)\b/, /\s+/, /[A-Za-z_$][\w$]*/],
      },
      {
        scope: { 1: 'keyword', 3: 'title.class' },
        begin: [/\b(?:contract|interface|library|struct|enum|event|error)\b/, /\s+/, /[A-Za-z_$][\w$]*/],
      },
    ],
  };
};

const hljsLanguages = {
  bash, c, cpp, css, diff, go, ini, java, javascript, json, markdown,
  plaintext, python, rust, solidity, sql, typescript, xml, yaml,
};
for (const [name, def] of Object.entries(hljsLanguages)) {
  hljs.registerLanguage(name, def);
}
hljs.registerLanguage('toml', ini);

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
// diagram source; hydrateMermaid() replaces successful hosts with SVG only,
// using securityLevel: 'strict' (mermaid's XSS sanitizer — never disabled).
// mermaid.render output is sanitized through mermaidViewer's
// sanitizeMermaidSvg BEFORE it is cached, and only sanitized SVG templates
// are cached (keyed by exact source + mermaid version/config/security
// identity) so session-switch remounts rehydrate without a second
// mermaid.render; DOM nodes and in-flight promises are never stored. Each
// host inserts a unique-ID instantiation of the sanitized template; SVG
// parsing fails closed (DOMParser only — no innerHTML fallback) and a failed
// instantiation degrades to the escaped, redacted source fallback, never raw
// markup. Both phases enforce the Rust MAX_MERMAID_SOURCE_BYTES bound: an
// oversize diagram is rejected BEFORE mermaid.render() and degrades to an
// escaped, redacted, bounded `Mermaid · source fallback` block.
// Failed/oversize diagrams are never eval'd, never parsed unboundedly, never
// cached, and errors never escape the render path; each render id's temp
// wrapper is cleaned on both outcomes. Successful hosts additionally carry
// the always-visible viewer toolbar row (mermaidViewer.ts: View / Download
// SVG; double-click opens the one global full-screen viewer).

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
    // CommonMark: a fence opener/closer may be indented 0-3 spaces. Model
    // output regularly indents fences (observed: " ```mermaid" inside a
    // briefing) — without tolerance the whole diagram renders as prose.
    const fence = line.match(/^ {0,3}```(\S*)\s*$/);
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

/* ------------------------------------------------------------------ *
 * Mermaid input/source + bounded rendered-host policy
 *
 * The Web host is a two-phase surface: renderFence() emits a placeholder
 * `.md-mermaid-host` (sync, streaming-safe), and hydrateMermaid() replaces
 * it with rendered SVG (async, post-mount). Both phases enforce the same
 * source byte bound, mirroring the Rust markdown renderer's
 * MAX_MERMAID_SOURCE_BYTES (pi-coding/src/markdown/mermaid.rs). The Web does
 * NOT duplicate the Rust graph parser (nodes/edges/cells): it only mirrors
 * the source byte limit and the success/failure terms. Diagrams above the
 * bound are rejected BEFORE mermaid.render() and degrade to an escaped,
 * redacted, bounded `Mermaid · source fallback` block — never parsed, never
 * eval'd, and (because no renderId is allocated) never leaking a temp
 * wrapper. strict security (securityLevel: 'strict' + escapeHtml, which
 * redacts) and per-render-id wrapper cleanup are preserved on the render
 * path.
 * ------------------------------------------------------------------ */

/// Source byte bound — matches the Rust MAX_MERMAID_SOURCE_BYTES exactly.
const MAX_MERMAID_SOURCE_BYTES = 64 * 1024;

/// Bound the fallback's displayed source and reason so an oversized (or merely
/// large) diagram never inflates the host DOM. The Rust TUI fallback shows the
/// full source because its surface is itself line-bounded; the Web host is an
/// unbounded DOM subtree, so the displayed source is capped with a marker and
/// the reason is capped to a single line.
const MERMAID_FALLBACK_SOURCE_CHARS = 4 * 1024;
const MERMAID_FALLBACK_REASON_CHARS = 512;
const MERMAID_FALLBACK_TRUNCATION = '\n… (source truncated)';

/**
 * UTF-8 byte length of a string — the unit of the Rust source bound. ASCII
 * fast path (UTF-16 length === UTF-8 byte length) skips the TextEncoder pass;
 * only the rare non-ASCII diagram pays for it. Matches the Rust byte limit
 * exactly so a multi-byte source is rejected at the same threshold both
 * surfaces use.
 */
function mermaidSourceBytes(source: string): number {
  return /[^\u0000-\u007f]/.test(source)
    ? new TextEncoder().encode(source).length
    : source.length;
}


/**
 * Bound a string for fallback display: cap to `limit` chars (JS slicing never
 * splits a code point) and append the truncation marker when trimming. The
 * marker is shared by the source and reason so a host never carries an
 * unbounded fragment.
 */
function boundForFallback(text: string, limit: number): string {
  if (text.length <= limit) return text;
  return text.slice(0, limit) + MERMAID_FALLBACK_TRUNCATION;
}

/**
 * Build the escaped, redacted, bounded `Mermaid · source fallback` block used
 * for both oversize-input rejection (before mermaid.render) and render-error
 * degradation. escapeHtml() runs redactSecrets FIRST, then escapes — so the
 * fallback never carries raw secrets or unescaped model characters. The
 * note/reason/source are all bounded so the host output stays bounded
 * regardless of input size.
 */
function mermaidSourceFallbackHtml(source: string, reason: string): string {
  const boundedReason = boundForFallback(reason, MERMAID_FALLBACK_REASON_CHARS);
  const boundedSource = boundForFallback(source, MERMAID_FALLBACK_SOURCE_CHARS);
  return (
    '<div class="md-mermaid-error">' +
    '<span class="md-mermaid-error__note">Mermaid · source fallback</span>' +
    `<span class="md-mermaid-error__reason">Could not render: ${escapeHtml(boundedReason)}</span>` +
    `<pre class="md-mermaid-error__source">${escapeHtml(boundedSource)}</pre>` +
    '</div>'
  );
}

function plantumlUnconfiguredHtml(source: string): string {
  const highlighted = escapeHtml(source);
  return (
    `<div class="md-fence md-fence--plantuml">` +
    `<div class="md-fence__head"><span class="md-fence__lang">plantuml</span>` +
    `<span class="md-fence__hint">配置 PlantUML server 后可渲染</span>` +
    `<button type="button" class="md-fence__copy">copy</button></div>` +
    `<pre class="md-fence__pre"><code class="md-code hljs">${highlighted}</code></pre>` +
    `</div>`
  );
}

function plantumlSourceFallbackHtml(source: string, reason: string): string {
  const boundedReason = boundForFallback(reason, MERMAID_FALLBACK_REASON_CHARS);
  const boundedSource = boundForFallback(source, MERMAID_FALLBACK_SOURCE_CHARS);
  return (
    '<div class="md-mermaid-error">' +
    '<span class="md-mermaid-error__note">PlantUML · source fallback</span>' +
    `<span class="md-mermaid-error__reason">Could not render: ${escapeHtml(boundedReason)}</span>` +
    `<pre class="md-mermaid-error__source">${escapeHtml(boundedSource)}</pre>` +
    '</div>'
  );
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
  if (baseLang === 'plantuml' || baseLang === 'puml') {
    if (mermaidSourceBytes(source) > MAX_MERMAID_SOURCE_BYTES) {
      return (
        '<div class="md-mermaid-host md-mermaid-host--error" data-diagram="plantuml" data-mermaid="done">' +
        plantumlSourceFallbackHtml(
          source,
          `source is ${mermaidSourceBytes(source)} bytes; limit is ${MAX_MERMAID_SOURCE_BYTES} bytes`,
        ) +
        '</div>'
      );
    }
    return `<div class="md-mermaid-host" data-diagram="plantuml">${escapeHtml(source)}</div>`;
  }
  if (baseLang === 'mermaid') {
    // Input/source policy: reject an oversize diagram at the input phase,
    // mirroring the Rust MAX_MERMAID_SOURCE_BYTES byte bound (the Web does
    // not duplicate the graph parser — only the source byte limit). Emit
    // the bounded `Mermaid · source fallback` host, claimed `done` and
    // marked error, so hydrateMermaid() never reaches mermaid.render() for
    // an oversize diagram and the streaming surface never builds a giant
    // host DOM. Normal-size diagrams keep the async hydration host.
    if (mermaidSourceBytes(source) > MAX_MERMAID_SOURCE_BYTES) {
      return (
        '<div class="md-mermaid-host md-mermaid-host--error" data-mermaid="done">' +
        mermaidSourceFallbackHtml(
          source,
          `source is ${mermaidSourceBytes(source)} bytes; limit is ${MAX_MERMAID_SOURCE_BYTES} bytes`,
        ) +
        '</div>'
      );
    }
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
  if (url === '' || url.charAt(0) === '#') return url;

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

function protectGeneratedMarkup(html: string, slots: string[]): string {
  return html.replace(
    /<code class="md-code"[^>]*>[\s\S]*?<\/code>|<img class="md-image"[^>]*>|<a\b[^>]*>[\s\S]*?<\/a>|<button\b[^>]*>[\s\S]*?<\/button>/g,
    (m) => {
      slots.push(m);
      return `\u0001L${slots.length - 1}\u0002`;
    },
  );
}

function restoreGeneratedMarkup(html: string, slots: string[]): string {
  return html.replace(/\u0001L(\d+)\u0002/g, (m, index: string) => {
    const slot = slots[Number(index)];
    return slot ?? m;
  });
}

const BARE_HTTP_TRAILING: Record<string, true> = {
  '!': true,
  '?': true,
  ']': true,
};

function peelHttpTrailing(token: string): { core: string; trailing: string } {
  let core = token;
  let trailing = '';
  while (core.length > 0) {
    const last = core.charAt(core.length - 1);
    if (last === ')') {
      const open = (core.match(/\(/g) ?? []).length;
      const close = (core.match(/\)/g) ?? []).length;
      if (close > open) {
        trailing = last + trailing;
        core = core.slice(0, -1);
        continue;
      }
      break;
    }
    if (WORKSPACE_PATH_TRAILING[last] === true || BARE_HTTP_TRAILING[last] === true) {
      trailing = last + trailing;
      core = core.slice(0, -1);
      continue;
    }
    break;
  }
  return { core, trailing };
}

function isBareHttpUrl(url: string): boolean {
  if (!/^https?:\/\/[A-Za-z0-9]/i.test(url)) return false;
  return safeUrl(url) === url;
}

function httpDownloadButton(target: string): string {
  return /^https?:/i.test(target)
    ? `<button type="button" class="md-link-download" data-href="${target}">Download</button>`
    : '';
}

function linkifyBareHttp(escaped: string): string {
  return escaped.replace(/(?<![A-Za-z0-9])(https?:\/\/[^\s<]+)/gi, (full, raw: string) => {
    const { core, trailing } = peelHttpTrailing(raw);
    if (!isBareHttpUrl(core)) return full;
    return `<a href="${core}" rel="noopener noreferrer">${core}</a>${httpDownloadButton(core)}${trailing}`;
  });
}


function decodeInlineCodeText(escaped: string): string {
  return escaped
    .replace(/&amp;/g, '&')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'");
}

type MarkdownNearbyPathResolution = { kind: 'none' | 'ambiguous' } | { kind: 'unique'; completion: string };

type CompleteMarkdownPath = (path: string) => MarkdownNearbyPathResolution;

function inlineCodeHtml(escaped: string, completePath: CompleteMarkdownPath): string {
  const candidate = parseWorkspacePathCandidate(decodeInlineCodeText(escaped));
  if (!candidate) return `<code class="md-code">${escaped}</code>`;
  const path = candidate.path;
  const bare = !path.includes('/') && !path.includes('\\');
  const resolution = completePath(path);
  const completion = resolution.kind === 'unique' ? resolution.completion : undefined;
  const lineAttr = candidate.line === undefined ? '' : ` data-md-line="${candidate.line}"`;
  if (bare) {
    if (resolution.kind === 'ambiguous') {
      return `<code class="md-code" data-md-resolved="none">${escaped}</code>`;
    }
    // Invariant: ambiguous context stays render-unresolved and inert; absent
    // context stamps the exact candidate — the basename is never guessed.
    const stamped = completion && completion !== path ? completion : path;
    return `<code class="md-code md-code-path" data-md-path="${escapeHtmlRaw(stamped)}"${lineAttr}>${escaped}</code>`;
  }
  const completionAttr = completion && completion !== path
    ? ` data-md-completion="${escapeHtmlRaw(completion)}"`
    : '';
  return `<code class="md-code md-code-path" data-md-path="${escapeHtmlRaw(path)}"${lineAttr}${completionAttr}>${escaped}</code>`;
}

function inlineMd(escaped: string, completePath: CompleteMarkdownPath): string {
  let s = escaped;
  s = s.replace(/`([^`]+)`/g, (_m, t: string) => inlineCodeHtml(t, completePath));
  // Images before links: ![alt](url) must not be consumed by the link rule.
  s = s.replace(/!\[([^\]]*)\]\(([^)]+)\)/g, (m, alt: string, url: string) => {
    const src = imageSrc(url);
    return src
      ? `<img class="md-image" src="${src}" alt="${alt}" loading="lazy" rel="noopener noreferrer">`
      : m; // invalid URL: keep the original (escaped) markdown literal
  });
  s = s.replace(/\[([^\]]+)\]\(([^)]+)\)/g, (_m, label: string, url: string) => {
    let target = safeUrl(url);
    // Workspace-relative `path` / `path:line` is not a safeUrl relative form
    // (`./`, `../`, `/`, `#`). Href authority rejects every RFC `scheme:`
    // form (including `ftp:12`) so this fallback cannot re-enable what
    // safeUrl rejected; code spans keep Makefile:12 / file.ext:line.
    if (!target) {
      const hrefCandidate = parseWorkspacePathCandidate(url, 'href');
      if (hrefCandidate && (hrefCandidate.path.includes('/') || hrefCandidate.path.includes('\\'))) {
        target = hrefCandidate.line === undefined ? hrefCandidate.path : `${hrefCandidate.path}:${hrefCandidate.line}`;
      }
    }
    if (!target) return label;
    // http(s) links keep safe navigation AND gain an explicit Download
    // affordance (fetch+blob only on click — never auto-download).
    return `<a href="${target}" rel="noopener noreferrer">${label}</a>${httpDownloadButton(target)}`;
  });
  const slots: string[] = [];
  s = protectGeneratedMarkup(s, slots);
  s = linkifyBareHttp(s);
  s = restoreGeneratedMarkup(s, slots);
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

function tryTable(lines: string[], i: number, completePath: CompleteMarkdownPath): { html: string; consumed: number } | null {
  if (!lines[i].includes('|')) return null;
  if (i + 1 >= lines.length || !isTableSeparator(lines[i + 1])) return null;
  const header = splitCells(lines[i]);
  const body: string[][] = [];
  let j = i + 2;
  while (j < lines.length && lines[j].trim() !== '' && lines[j].includes('|')) {
    body.push(splitCells(lines[j]));
    j++;
  }
  const headRow = `<tr>${header.map((c) => `<th scope="col">${inlineMd(c, completePath)}</th>`).join('')}</tr>`;
  const bodyRows = body
    .map((row) => `<tr>${row.map((c) => `<td>${inlineMd(c, completePath)}</td>`).join('')}</tr>`)
    .join('');
  return {
    html: `<div class="md-table-wrap"><table class="md-table"><thead>${headRow}</thead>${bodyRows ? `<tbody>${bodyRows}</tbody>` : ''}</table></div>`,
    consumed: j - i,
  };
}

interface ListLevel {
  tag: 'ul' | 'ol';
  indent: number;
}

function renderLines(escaped: string, completePath: CompleteMarkdownPath): string {
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

    const table = tryTable(lines, i, completePath);
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
      html += `<h${level} class="md-h${level}">${inlineMd(heading[2], completePath)}</h${level}>`;
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
        quote.push(inlineMd(lines[i].replace(/^&gt;\s?/, ''), completePath));
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
      html += `<li>${inlineMd(content, completePath)}`;
      continue;
    }

    closeAllLists();
    if (line.trim() !== '') {
      html += `<p class="md-p">${inlineMd(line, completePath)}</p>`;
    }
  }
  closeAllLists();
  return html;
}

/* ------------------------------------------------------------------ *
 * Public render API
 * ------------------------------------------------------------------ */

/** Render raw markdown (table, math, mermaid, task lists, fences, ...). */
export function renderMarkdown(raw: string, pathContext: string = raw): string {
  const completions = new Map<string, MarkdownNearbyPathResolution>();
  const completePath: CompleteMarkdownPath = (path) => {
    const cached = completions.get(path);
    if (cached !== undefined) return cached;
    const completed = markdownNearbyPathResolution(path, pathContext);
    completions.set(path, completed);
    return completed;
  };
  const segments = extractFences(raw);
  let html = '';
  for (const segment of segments) {
    if (segment.kind === 'fence') {
      html += renderFence(segment.lang, segment.source);
      continue;
    }
    const { text, parts } = extractMath(segment.text);
    const escaped = escapeHtml(text);
    html += substituteMath(renderLines(escaped, completePath), parts);
  }
  return html;
}

/** Render the complete assistant text accumulated so far. Streaming callers
 * replace their provisional HTML with this result, so every chunk boundary
 * recomputes the unstable tail through the ordinary parser/sanitizer. */
export function renderStreamingMarkdown(raw: string, pathContext: string = raw): string {
  return renderMarkdown(raw, pathContext);
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

/** Final/restored thinking body: newline-normalize, then markdown. */
export function thinkingBodyInnerHtml(thinking: string, pathContext?: string): string {
  const normalized = normalizeThinkingNewlines(thinking);
  return renderMarkdown(normalized, pathContext ?? normalized);
}

/** Paint thinking before other blocks; does not mutate storage. */
export function thinkingFirstBlocks(blocks: readonly (ContentBlock | null | undefined)[]): ContentBlock[] {
  const thinking: ContentBlock[] = [];
  const rest: ContentBlock[] = [];
  for (const block of blocks) {
    if (!block) continue;
    if (block.type === 'thinking') thinking.push(block);
    else rest.push(block);
  }
  return thinking.concat(rest);
}

/** Render one assistant message's content blocks (final, non-streaming). */
export function renderBlocks(blocks: ContentBlock[], pathContext?: string): string {
  const context = pathContext ?? blocks
    .map((block) => (block && block.type === 'text' && typeof block.text === 'string' ? block.text : ''))
    .filter((text) => text !== '')
    .join('\n');
  let html = '';
  for (const block of thinkingFirstBlocks(blocks)) {
    if (!block) continue;
    if (block.type === 'text' && typeof block.text === 'string' && block.text !== '') {
      html += renderMarkdown(block.text, context);
    } else if (block.type === 'thinking' && typeof block.thinking === 'string') {
      html += `<details class="thinking" open><summary class="thinking__summary">${thinkingSummaryHtml()}</summary><div class="thinking__body">${thinkingBodyInnerHtml(block.thinking, context)}</div></details>`;
    } else if (block.type === 'image') {
      const src = safeImage(block.mimeType, block.data);
      if (src) html += `<img class="md-image" src="${src}" alt="image">`;
    }
  }
  return html;
}

/* ------------------------------------------------------------------ *
 * Mermaid hydration (async, post-mount)
 * ------------------------------------------------------------------ */

let mermaidSeq = 0;

/// Pinned to the mermaid package the Web client ships (`package.json`).
/// Part of the SVG cache key so a version bump cannot reuse old SVG.
const MERMAID_VERSION = '11.16.1';

const MERMAID_THEME_VARIABLES = {
  background: '#0f1216',
  primaryColor: '#151820',
  secondaryColor: '#1f252d',
  tertiaryColor: '#1f252d',
  primaryTextColor: '#e8ecf4',
  secondaryTextColor: '#e8ecf4',
  tertiaryTextColor: '#e8ecf4',
  primaryBorderColor: '#3a4350',
  secondaryBorderColor: '#3a4350',
  tertiaryBorderColor: '#3a4350',
  lineColor: '#9ca3b0',
  arrowheadColor: '#9ca3b0',
  textColor: '#e8ecf4',
  mainBkg: '#151820',
  secondBkg: '#1f252d',
  mainContrastColor: '#e8ecf4',
  border1: '#3a4350',
  border2: '#3a4350',
  nodeBkg: '#151820',
  nodeBorder: '#3a4350',
  clusterBkg: '#1f252d',
  clusterBorder: '#3a4350',
  titleColor: '#00b4ff',
  edgeLabelBackground: '#151820',
  labelBackground: '#151820',
  actorBorder: '#3a4350',
  actorBkg: '#151820',
  actorTextColor: '#e8ecf4',
  actorLineColor: '#3a4350',
  signalColor: '#9ca3b0',
  signalTextColor: '#e8ecf4',
  labelBoxBkgColor: '#151820',
  labelBoxBorderColor: '#3a4350',
  labelTextColor: '#e8ecf4',
  loopTextColor: '#e8ecf4',
  noteBorderColor: '#3a4350',
  noteBkgColor: '#1f252d',
  noteTextColor: '#e8ecf4',
  activationBorderColor: '#3a4350',
  activationBkgColor: '#1f252d',
  sequenceNumberColor: '#e8ecf4',
  classText: '#e8ecf4',
  useGradient: false,
  pieStrokeColor: '#3a4350',
  pieOuterStrokeColor: '#3a4350',
  pieTitleTextColor: '#e8ecf4',
  pieSectionTextColor: '#e8ecf4',
  pieLegendTextColor: '#e8ecf4',
  cScale0: '#151820',
  cScale1: '#1f252d',
  cScale2: '#2a3038',
  cScale3: '#0a0d10',
  cScale4: '#151820',
  cScale5: '#1f252d',
  cScale6: '#2a3038',
  cScale7: '#0a0d10',
  cScale8: '#151820',
  cScale9: '#1f252d',
  cScale10: '#2a3038',
  cScale11: '#0a0d10',
};

const MERMAID_INIT_CONFIG = {
  startOnLoad: false,
  securityLevel: 'strict' as const,
  theme: 'dark' as const,
  htmlLabels: false as const,
  themeVariables: MERMAID_THEME_VARIABLES,
};

const MERMAID_AUTHOR_INIT_CONFIG = {
  startOnLoad: false,
  securityLevel: 'strict' as const,
  theme: 'default' as const,
  htmlLabels: false as const,
  themeVariables: {} as Record<string, string | boolean>,
};

export type MermaidPalette = 'theme' | 'author';

const MERMAID_AUTHOR_CLASSDEF = /(?:^|\n)\s*classDef\s+/i;
const MERMAID_AUTHOR_STYLE = /(?:^|\n)\s*(?:link)?style\s+\S/i;
const MERMAID_AUTHOR_INIT = /%%\{\s*init\b[^\n]*(?:theme|themeVariables|themeCSS)/i;

export function mermaidSourcePalette(source: string): MermaidPalette {
  MERMAID_AUTHOR_CLASSDEF.lastIndex = 0;
  MERMAID_AUTHOR_STYLE.lastIndex = 0;
  MERMAID_AUTHOR_INIT.lastIndex = 0;
  if (MERMAID_AUTHOR_CLASSDEF.test(source)) return 'author';
  if (MERMAID_AUTHOR_STYLE.test(source)) return 'author';
  if (MERMAID_AUTHOR_INIT.test(source)) return 'author';
  return 'theme';
}

export function prepareMermaidSource(source: string): string {
  return source
    .replace(/[\u201c\u201d]/g, "'")
    .replace(
      /"((?:\\.|[^"\\])*)"((?:\s*<br\s*\/?>)+)([^"\n]*)"/gi,
      (_match, inner: string, breaks: string, rest: string) => {
        const joined = breaks.replace(/<br\s*\/?>/gi, '<br>');
        return `"${inner}${joined}${rest}"`;
      },
    );
}

function mermaidInitConfig(palette: MermaidPalette) {
  return palette === 'author' ? MERMAID_AUTHOR_INIT_CONFIG : MERMAID_INIT_CONFIG;
}

export function mermaidRenderIdentity(palette: MermaidPalette): string {
  const config = mermaidInitConfig(palette);
  return (
    MERMAID_VERSION +
    '\0' +
    config.securityLevel +
    '\0' +
    config.theme +
    '\0' +
    String(config.htmlLabels) +
    '\0' +
    JSON.stringify(config.themeVariables)
  );
}

let mermaidAppliedPalette: MermaidPalette | null = null;
let mermaidRenderBusy = false;
const mermaidRenderWaiters: Array<() => void> = [];

function applyMermaidPalette(palette: MermaidPalette): void {
  if (mermaidAppliedPalette === palette) return;
  const config = mermaidInitConfig(palette);
  mermaid.initialize({
    startOnLoad: config.startOnLoad,
    securityLevel: config.securityLevel,
    theme: config.theme,
    htmlLabels: config.htmlLabels,
    themeVariables: config.themeVariables,
  });
  mermaidAppliedPalette = palette;
}

function runExclusiveMermaidRender<T>(work: () => Promise<T>): Promise<T> {
  if (!mermaidRenderBusy) {
    mermaidRenderBusy = true;
    const run = work();
    const release = () => {
      mermaidRenderBusy = false;
      const next = mermaidRenderWaiters.shift();
      if (next !== undefined) next();
    };
    void run.then(release, release);
    return run;
  }
  return new Promise<T>((resolve, reject) => {
    mermaidRenderWaiters.push(() => {
      runExclusiveMermaidRender(work).then(resolve, reject);
    });
  });
}

function initMermaid(): void {
  if (mermaidRenderBusy) return;
  applyMermaidPalette('theme');
}

/**
 * Remove mermaid's temporary render wrapper for one render id.
 *
 * mermaidAPI.render() (v11) appends `<div id="d<id>"><svg id="<id>">…</svg></div>`
 * to `document.body` and removes it via its own `removeTempElements()` on
 * SUCCESS. On a parse error it draws the parser-error diagram ("Syntax error
 * in text", "mermaid version …") into that wrapper and THROWS before the
 * removal runs — the wrapper then stays in the page below the
 * composer/footer, one leaked div per failed render. This mirrors mermaid's
 * own removal, scoped to the exact render id we generated, and verifies the
 * wrapper is mermaid's (it contains the svg with the render id) so nothing
 * else is touched. We run securityLevel 'strict' (never sandbox), so the
 * `d<id>` div is the only artifact to remove. No-op when there is no DOM
 * (unit-test environment) or no wrapper.
 */
function removeMermaidTempDom(renderId: string): void {
  if (typeof document === 'undefined') return;
  // The wrapper div holds the `<svg id="<renderId>">` mermaid drew into;
  // remove exactly that pair.
  const div = document.getElementById(`d${renderId}`);
  if (div?.querySelector(`#${renderId}`)) div.remove();
}

/* ------------------------------------------------------------------ *
 * Bounded mermaid SVG result cache (session-switch rehydration)
 *
 * Values are sanitized SVG *templates* only — sanitized through mermaid
 * Viewer's sanitizeMermaidSvg at the cache boundary (renderMermaidSvgCached),
 * never DOM nodes, never the in-flight mermaid.render promise (that
 * promise's render id owns a temp wrapper). Every host instantiates the
 * sanitized template with a unique ID namespace so two simultaneous
 * identical diagrams cannot collide.
 * Errors are not stored: a failed diagram retries on the next host.
 * Concurrent callers of the same source+identity share one promise;
 * a rejected promise is dropped so the next hydrate is not sticky.
 * ------------------------------------------------------------------ */

const MERMAID_SVG_CACHE_MAX_ENTRIES = 64;
const MERMAID_SVG_CACHE_MAX_BYTES = 2 * 1024 * 1024;
const MERMAID_XLINK_NS = 'http://www.w3.org/1999/xlink';
const MERMAID_IDREF_ATTRS: Record<string, true> = {
  'aria-labelledby': true,
  'aria-describedby': true,
  'aria-owns': true,
  'aria-controls': true,
  'aria-flowto': true,
  'aria-activedescendant': true,
  'aria-details': true,
  'aria-errormessage': true,
};
const MERMAID_URL_ATTRS: Record<string, true> = {
  marker: true,
  'marker-start': true,
  'marker-mid': true,
  'marker-end': true,
  'clip-path': true,
  mask: true,
  filter: true,
  fill: true,
  stroke: true,
  style: true,
};

interface MermaidSvgCacheEntry {
  svg: string;
  bytes: number;
}

const mermaidSvgCache = new Map<string, MermaidSvgCacheEntry>();
const mermaidSvgInflight = new Map<string, Promise<string>>();
let mermaidSvgCacheBytes = 0;
let mermaidSvgCacheMaxEntries = MERMAID_SVG_CACHE_MAX_ENTRIES;
let mermaidSvgCacheMaxBytes = MERMAID_SVG_CACHE_MAX_BYTES;
let mermaidCacheIdentityOverride: string | null = null;
let mermaidSvgCacheEpoch = 0;
let mermaidInstanceSeq = 0n;

function mermaidSvgCacheKey(source: string): string {
  const identity = mermaidCacheIdentityOverride
    ?? mermaidRenderIdentity(mermaidSourcePalette(source));
  return identity + '\0' + source;
}

function mermaidSvgCacheGet(source: string): string | undefined {
  const key = mermaidSvgCacheKey(source);
  const entry = mermaidSvgCache.get(key);
  if (!entry) return undefined;
  mermaidSvgCache.delete(key);
  mermaidSvgCache.set(key, entry);
  return entry.svg;
}

function mermaidSvgCacheEvict(): void {
  while (
    mermaidSvgCache.size > mermaidSvgCacheMaxEntries
    || mermaidSvgCacheBytes > mermaidSvgCacheMaxBytes
  ) {
    const oldest = mermaidSvgCache.keys().next().value;
    if (oldest === undefined) break;
    const entry = mermaidSvgCache.get(oldest);
    mermaidSvgCache.delete(oldest);
    if (entry) mermaidSvgCacheBytes -= entry.bytes;
  }
}

/** Charge retained UTF-16 bytes of the identity+source key AND the SVG. */
function mermaidSvgCachePut(key: string, svg: string): void {
  if (typeof svg !== 'string' || !svg.includes('<svg') || /<script\b/i.test(svg)) return;
  const bytes = (key.length + svg.length) * 2;
  if (bytes > mermaidSvgCacheMaxBytes) return;
  const existing = mermaidSvgCache.get(key);
  if (existing) {
    mermaidSvgCacheBytes -= existing.bytes;
    mermaidSvgCache.delete(key);
  }
  mermaidSvgCache.set(key, { svg, bytes });
  mermaidSvgCacheBytes += bytes;
  mermaidSvgCacheEvict();
}

function nextMermaidInstancePrefix(): string {
  mermaidInstanceSeq += 1n;
  return `mmd-i${mermaidInstanceSeq}-`;
}

function mermaidSvgElements(root: Element): Element[] {
  const out: Element[] = [root];
  if (typeof root.querySelectorAll === 'function') {
    out.push(...Array.from(root.querySelectorAll('*')));
  }
  return out;
}

function mermaidSvgAttrName(attr: Attr): string {
  return (attr.localName || attr.name || '').toLowerCase();
}

function mermaidSvgAttrAt(attrs: NamedNodeMap, index: number): Attr | null {
  return typeof attrs.item === 'function' ? attrs.item(index) : null;
}

function mermaidSvgSetAttr(el: Element, attr: Attr, value: string): void {
  const ns = attr.namespaceURI;
  if (ns && typeof el.setAttributeNS === 'function') {
    el.setAttributeNS(ns, attr.name, value);
    return;
  }
  el.setAttribute(attr.name, value);
}



function rewriteMermaidUrlTokens(value: string, mapped: Record<string, string>): string {
  return value.replace(
    /url\(\s*(['"]?)#([^)'"\s]+)\1\s*\)/gi,
    (all, quote: string, id: string) => {
      const next = mapped[id];
      return next ? `url(${quote}#${next}${quote})` : all;
    },
  );
}

function rewriteMermaidIdRefs(value: string, mapped: Record<string, string>): string {
  return value.split(/(\s+)/).map((token) => (
    /^\s+$/.test(token) ? token : (mapped[token] ?? token)
  )).join('');
}

function rewriteMermaidHref(value: string, mapped: Record<string, string>): string {
  if (value.charAt(0) !== '#') return value;
  const next = mapped[value.slice(1)];
  return next ? `#${next}` : value;
}

/** Exact-safe CSS rewrite: skip comments and quoted strings; rewrite `#knownId` idents only. */
function rewriteMermaidCssLocalIds(css: string, mapped: Record<string, string>): string {
  let out = '';
  let i = 0;
  while (i < css.length) {
    if (css.startsWith('/*', i)) {
      const end = css.indexOf('*/', i + 2);
      const block = end < 0 ? css.slice(i) : css.slice(i, end + 2);
      out += block;
      i += block.length;
      continue;
    }
    if (css.slice(i, i + 4).toLowerCase() === 'url(') {
      let j = i + 4;
      let quote = '';
      while (j < css.length) {
        const current = css.charAt(j);
        if (quote !== '') {
          if (current === '\\') { j += 2; continue; }
          if (current === quote) quote = '';
        } else if (current === '"' || current === "'") {
          quote = current;
        } else if (current === ')') {
          j += 1;
          break;
        }
        j += 1;
      }
      out += rewriteMermaidUrlTokens(css.slice(i, j), mapped);
      i = j;
      continue;
    }
    const ch = css.charAt(i);
    if (ch === '"' || ch === "'") {
      let j = i + 1;
      while (j < css.length) {
        if (css.charAt(j) === '\\') { j += 2; continue; }
        if (css.charAt(j) === ch) { j += 1; break; }
        j += 1;
      }
      out += css.slice(i, j);
      i = j;
      continue;
    }
    if (ch === '#') {
      let j = i + 1;
      while (j < css.length && /[\w-]/.test(css.charAt(j))) j += 1;
      const id = css.slice(i + 1, j);
      const next = mapped[id];
      if (next) {
        out += `#${next}`;
        i = j;
        continue;
      }
    }
    out += ch;
    i += 1;
  }
  return out;
}

/**
 * Parse an SVG string into a DOM root, or null when nothing may be touched:
 * no DOMParser (plain-node), a parse failure, or a parsererror document.
 * FAILS CLOSED — there is deliberately no `innerHTML` parsing fallback: a
 * document that DOMParser rejects is never re-parsed through the looser
 * HTML path (which would materialize raw markup).
 */
function parseMermaidSvgRoot(svg: string): Element | null {
  try {
    if (typeof DOMParser === 'undefined') return null;
    const doc = new DOMParser().parseFromString(svg, 'image/svg+xml');
    const root = doc.documentElement;
    if (root && !/parsererror/i.test(root.nodeName)) {
      const tag = (root.localName || root.tagName || '').toLowerCase();
      if (tag === 'svg') return root;
      const nested = typeof root.querySelector === 'function' ? root.querySelector('svg') : null;
      if (nested) return nested;
    }
  } catch {
    // fail closed
  }
  return null;
}

function serializeMermaidSvg(root: Element): string {
  if (typeof root.outerHTML === 'string' && root.outerHTML !== '') return root.outerHTML;
  if (typeof XMLSerializer !== 'undefined') {
    return new XMLSerializer().serializeToString(root);
  }
  return '';
}

/**
 * Instantiate a sanitized SVG template with a unique ID namespace.
 *
 * Parse/clone the template into a DOM tree, collect elements that actually
 * have an `id`, then rewrite only attributes: IDREF lists, fragment hrefs,
 * and local `url(#knownId)` paint/filter/marker/mask/clip/style tokens.
 * Text nodes, comments, and quoted CSS/string literals are never touched.
 * FAILS CLOSED: an unparsable (or unserializable) template yields '' — never
 * the raw template — so a failed instantiation can never put unsanitized
 * markup on the page.
 */
function instantiateMermaidSvg(svg: string): string {
  const root = parseMermaidSvgRoot(svg);
  if (!root) return '';
  const elements = mermaidSvgElements(root);
  const mapped: Record<string, string> = {};
  const prefix = nextMermaidInstancePrefix();
  for (const el of elements) {
    const id = el.getAttribute('id');
    if (id) mapped[id] = prefix + id;
  }
  if (Object.keys(mapped).length === 0) return svg;
  for (const el of elements) {
    const attrs = el.attributes;
    if (attrs && typeof attrs.length === 'number') {
      for (let i = 0; i < attrs.length; i += 1) {
        const attr = mermaidSvgAttrAt(attrs, i);
        if (!attr) continue;
        const name = mermaidSvgAttrName(attr);
        const value = attr.value;
        if (name === 'id') {
          const next = mapped[value];
          if (next) mermaidSvgSetAttr(el, attr, next);
          continue;
        }
        if (MERMAID_IDREF_ATTRS[name] === true) {
          const next = rewriteMermaidIdRefs(value, mapped);
          if (next !== value) mermaidSvgSetAttr(el, attr, next);
          continue;
        }
        if (name === 'href' || attr.name === 'xlink:href' || (attr.namespaceURI === MERMAID_XLINK_NS && name === 'href')) {
          const next = rewriteMermaidHref(value, mapped);
          if (next !== value) mermaidSvgSetAttr(el, attr, next);
          continue;
        }
        if (MERMAID_URL_ATTRS[name] === true) {
          const next = rewriteMermaidUrlTokens(value, mapped);
          if (next !== value) mermaidSvgSetAttr(el, attr, next);
        }
      }
    }
    if ((el.localName || el.tagName || '').toLowerCase() === 'style') {
      const text = el.textContent ?? '';
      const next = rewriteMermaidCssLocalIds(text, mapped);
      if (next !== text) el.textContent = next;
    }
  }
  const serialized = serializeMermaidSvg(root);
  return serialized;
}

/**
 * One-shot wide re-check for a rendered mermaid host that had no layout at
 * classification time (clientWidth 0 — hidden fold/tab/offscreen mount).
 * Deciding "wide" against a 0-width host would mark EVERY diagram wide, so
 * the decision is deferred: one ResizeObserver re-evaluates when the host
 * first gains a nonzero layout width (the initial observation fires even
 * without a resize, covering layout that lands between classification and
 * observe), then disconnects. A host removed from the document disconnects
 * too, so the observer is always bounded — never a polling or timer loop.
 */
function scheduleMermaidWideRecheck(host: HTMLElement): void {
  if (typeof ResizeObserver === 'undefined') return;
  const observer = new ResizeObserver(() => {
    if (!host.isConnected || host.clientWidth > 0) {
      observer.disconnect();
      if (host.isConnected) classifyMermaidWide(host);
    }
  });
  observer.observe(host);
}

/**
 * Wide classification for one rendered mermaid host against its LIVE layout
 * width: an intrinsic viewBox width larger than the host's viewport scrolls
 * horizontally (`data-mermaid-wide` + intrinsic min-width); a fitting
 * diagram stays centered by the shared `margin-inline: auto`. A host with
 * no layout yet (clientWidth 0) defers the decision — against width 0 every
 * diagram would look wide — and re-evaluates exactly once on visible
 * layout (scheduleMermaidWideRecheck).
 */
function classifyMermaidWide(host: Element): void {
  if (!('querySelector' in host)) return;
  const renderedSvg = host.querySelector('svg');
  if (!renderedSvg) return;
  const viewBox = renderedSvg.getAttribute('viewBox')?.trim().split(/\s+/);
  const width = viewBox?.length === 4 ? Number(viewBox[2]) : NaN;
  if (!(Number.isFinite(width) && width > 0)) return;
  const hostWidth = (host as HTMLElement).clientWidth;
  if (hostWidth <= 0) {
    // No layout yet: defer the decision, re-check once on visible layout.
    scheduleMermaidWideRecheck(host as HTMLElement);
    return;
  }
  if (width > hostWidth) {
    renderedSvg.setAttribute('data-mermaid-wide', 'true');
    renderedSvg.style.minWidth = `${width}px`;
  }
}

function applyRenderedMermaidSvg(host: Element, svg: string, source: string): void {
  const instance = instantiateMermaidSvg(svg);
  if (instance === '') {
    host.innerHTML = mermaidSourceFallbackHtml(source, 'sanitized SVG could not be instantiated');
    host.classList.add('md-mermaid-host--error');
    return;
  }
  const palette: MermaidPalette = host.getAttribute('data-diagram') === 'plantuml'
    || mermaidSourcePalette(source) === 'author'
    ? 'author'
    : 'theme';
  if (palette === 'author') host.setAttribute('data-mermaid-palette', 'author');
  else host.removeAttribute('data-mermaid-palette');
  const canvasStyle = palette === 'author'
    ? ` style="background:${MERMAID_AUTHOR_CANVAS_FILL}"`
    : '';
  host.innerHTML =
    MERMAID_HOST_TOOLBAR_HTML +
    `<div class="md-mermaid-viewport"${canvasStyle}>${instance}</div>`;
  if ('querySelector' in host) {
    const renderedSvg = host.querySelector('svg');
    renderedSvg?.removeAttribute('style');
    if (renderedSvg?.getAttribute('width') === '100%') {
      const viewBox = renderedSvg.getAttribute('viewBox')?.trim().split(/\s+/);
      if (viewBox?.length === 4) {
        const width = Number(viewBox[2]);
        const height = Number(viewBox[3]);
        if (Number.isFinite(width) && width > 0 && Number.isFinite(height) && height > 0) {
          renderedSvg.setAttribute('width', String(width));
          renderedSvg.setAttribute('height', String(height));
          classifyMermaidWide(host);
        }
      }
    }
    if (palette === 'author' && renderedSvg) {
      renderedSvg.style.background = MERMAID_AUTHOR_CANVAS_FILL;
    }
  }
  host.classList.add('md-mermaid-host--rendered');
}

/**
 * Render one diagram to a sanitized SVG template. Same source+identity
 * shares one mermaid.render promise; the temp wrapper for the allocated
 * render id is always removed, including when render rejects. Failed
 * promises are dropped so the next hydrate retries. Completions capture
 * the start-of-render key+epoch: a reset/identity change cannot admit
 * the old SVG, and cleanup removes only this promise from inflight.
 *
 * CACHE BOUNDARY: mermaid.render output is sanitized through mermaidViewer's
 * sanitizeMermaidSvg BEFORE it is cached or returned — only sanitized SVG
 * ever enters the cache. Sanitization failing closed ('') rejects the render,
 * so a diagram mermaid produced but that cannot be sanitized degrades to the
 * source fallback instead of ever reaching a host.
 */
function renderMermaidSvgCached(source: string): Promise<string> {
  const cached = mermaidSvgCacheGet(source);
  if (cached !== undefined) return Promise.resolve(cached);
  const key = mermaidSvgCacheKey(source);
  const pending = mermaidSvgInflight.get(key);
  if (pending) return pending;
  const epoch = mermaidSvgCacheEpoch;
  const renderId = `mmd-${mermaidSeq++}`;
  const palette = mermaidSourcePalette(source);
  const promise = runExclusiveMermaidRender(async () => {
    applyMermaidPalette(palette);
    const { svg } = await mermaid.render(renderId, prepareMermaidSource(source));
    const sanitized = sanitizeMermaidSvg(svg);
    if (sanitized === '') throw new Error('mermaid render output failed sanitization');
    if (epoch === mermaidSvgCacheEpoch) mermaidSvgCachePut(key, sanitized);
    return sanitized;
  });
  mermaidSvgInflight.set(key, promise);
  const cleanup = () => {
    removeMermaidTempDom(renderId);
    if (mermaidSvgInflight.get(key) === promise) mermaidSvgInflight.delete(key);
  };
  void promise.then(cleanup, cleanup);
  return promise;
}


/**
 * Test-only mermaid SVG cache controls. Production callers never import this.
 * `reset()` bumps the cache epoch (in-flight completions cannot put), drops
 * inflight, and restores default caps/identity unless overrides are passed.
 * `clear: false` retargets identity/caps against the live map so a
 * config-identity change is a miss without wiping prior keys.
 */
export const mermaidSvgCacheTest = {
  reset(options?: {
    maxEntries?: number;
    maxBytes?: number;
    identity?: string;
    clear?: boolean;
  }): void {
    mermaidSvgCacheEpoch += 1;
    mermaidSvgInflight.clear();
    const clear = options?.clear !== false;
    if (clear) {
      mermaidSvgCache.clear();
      mermaidSvgCacheBytes = 0;
    }
    mermaidSvgCacheMaxEntries = options?.maxEntries ?? MERMAID_SVG_CACHE_MAX_ENTRIES;
    mermaidSvgCacheMaxBytes = options?.maxBytes ?? MERMAID_SVG_CACHE_MAX_BYTES;
    mermaidCacheIdentityOverride = options?.identity ?? null;
    mermaidAppliedPalette = null;
    mermaidRenderBusy = false;
    mermaidRenderWaiters.length = 0;
    if (!clear) mermaidSvgCacheEvict();
    if (clear) configurePlantumlServer('');
  },
  bytes(): number {
    return mermaidSvgCacheBytes;
  },
  size(): number {
    return mermaidSvgCache.size;
  },
  instanceSeq(value?: bigint): bigint {
    if (value !== undefined) mermaidInstanceSeq = value;
    return mermaidInstanceSeq;
  },
};


async function renderPlantumlSvg(source: string, signal?: AbortSignal): Promise<string> {
  const encoded = await encodePlantumlSource(plantumlDocumentSource(source));
  const url = plantumlSvgRequestUrl(configuredPlantumlServer(), encoded);
  if (!url) throw new Error('plantuml server URL is invalid');
  if (typeof fetch !== 'function') throw new Error('plantuml fetch is unavailable');
  const response = await fetch(url, {
    method: 'GET',
    credentials: 'omit',
    signal,
    headers: { Accept: 'image/svg+xml' },
  });
  if (!response.ok) {
    const body = await response.text().catch(() => '');
    throw new Error(plantumlHttpErrorDetail(response.status, body));
  }
  const svg = await response.text();
  const sanitized = sanitizeMermaidSvg(svg);
  if (sanitized === '') throw new Error('plantuml SVG failed sanitization');
  return sanitized;
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
 * Cache hits apply the stored SVG/classes/data and fire `onMutated` once
 * in the same turn (no mermaid.render, no temp wrapper).
 *
 * `data-mermaid="done"` is a TERMINAL mark written only after a host was
 * actually settled (rendered SVG or a bounded fallback), so a pass cancelled
 * by `signal` leaves every host it had not yet mutated still claimable: a
 * replayed hydration (React StrictMode's setup→cleanup→setup, a superseded
 * body remount) renders the diagram instead of skipping a claim with no
 * render behind it.
 */
export async function hydrateMermaid(
  root: HTMLElement,
  onMutated?: () => void,
  signal?: AbortSignal,
): Promise<void> {
  if (signal?.aborted || !root.isConnected) return;
  try {
    initMermaid();
  } catch {
    return;
  }
  if (signal?.aborted || !root.isConnected) return;
  const hosts = Array.from(root.querySelectorAll('.md-mermaid-host'));
  /** Settle ONE host: mark it terminal (never to be hydrated again) and tell
   *  the caller the layout moved. The mark is taken ONLY here, after a
   *  mutation — never before the async render. A pass cancelled by its own
   *  signal (React StrictMode's setup→cleanup→setup effect replay, or any
   *  superseded body) must leave a connected host claimable, or the replay
   *  would skip a host that was never rendered. Keeping the mark off the
   *  in-flight path costs no duplicate render: renderMermaidSvgCached shares
   *  one in-flight render per source key and replays a cache hit for a
   *  repeat, and the first pass to mutate wins. */
  const settle = (host: Element) => {
    host.setAttribute('data-mermaid', 'done');
    onMutated?.();
  };
  for (const host of hosts) {
    if (signal?.aborted || !root.isConnected || !host.isConnected) return;
    if (host.getAttribute('data-mermaid') === 'done') continue;
    const source = host.textContent ?? '';
    if (source.trim() === '') {
      host.innerHTML = '';
      settle(host);
      continue;
    }
    if (host.getAttribute('data-diagram') === 'plantuml') {
      if (mermaidSourceBytes(source) > MAX_MERMAID_SOURCE_BYTES) {
        host.innerHTML = plantumlSourceFallbackHtml(
          source,
          `source is ${mermaidSourceBytes(source)} bytes; limit is ${MAX_MERMAID_SOURCE_BYTES} bytes`,
        );
        host.classList.add('md-mermaid-host--error');
        settle(host);
        continue;
      }
      if (configuredPlantumlServer() === '') {
        host.innerHTML = plantumlUnconfiguredHtml(source);
        settle(host);
        continue;
      }
      try {
        const svg = await renderPlantumlSvg(source, signal);
        if (signal?.aborted || !root.isConnected || !host.isConnected) return;
        applyRenderedMermaidSvg(host, svg, source);
      } catch (err) {
        if (signal?.aborted || !root.isConnected || !host.isConnected) return;
        const reason = err instanceof Error && err.message ? err.message : 'render failed';
        host.innerHTML = plantumlSourceFallbackHtml(source, reason);
        host.classList.add('md-mermaid-host--error');
      }
      if (signal?.aborted || !root.isConnected || !host.isConnected) return;
      settle(host);
      continue;
    }
    // Rendered-host policy: reject an oversize diagram BEFORE mermaid.render.
    // The input phase (renderFence) already emits a fallback for oversize,
    // but a host may carry oversize source via a direct create/restore path,
    // so this is defense-in-depth: never call mermaid.render() on an oversize
    // diagram (it would parse unboundedly and leak its temp wrapper), and
    // take the bounded escaped/redacted fallback instead. No renderId is
    // allocated on this path, so no wrapper cleanup is needed.
    if (mermaidSourceBytes(source) > MAX_MERMAID_SOURCE_BYTES) {
      host.innerHTML = mermaidSourceFallbackHtml(
        source,
        `source is ${mermaidSourceBytes(source)} bytes; limit is ${MAX_MERMAID_SOURCE_BYTES} bytes`,
      );
      host.classList.add('md-mermaid-host--error');
      settle(host);
      continue;
    }
    const cached = mermaidSvgCacheGet(source);
    if (cached !== undefined) {
      applyRenderedMermaidSvg(host, cached, source);
      settle(host);
      continue;
    }
    try {
      const svg = await renderMermaidSvgCached(source);
      if (signal?.aborted || !root.isConnected || !host.isConnected) return;
      applyRenderedMermaidSvg(host, svg, source);
    } catch (err) {
      if (signal?.aborted || !root.isConnected || !host.isConnected) return;
      const reason = err instanceof Error && err.message ? err.message : 'render failed';
      host.innerHTML = mermaidSourceFallbackHtml(source, reason);
      host.classList.add('md-mermaid-host--error');
    }
    if (signal?.aborted || !root.isConnected || !host.isConnected) return;
    settle(host);
  }
}

subscribePlantumlServer(() => {
  if (typeof document === 'undefined' || typeof document.querySelectorAll !== 'function') return;
  const hosts = document.querySelectorAll('.md-mermaid-host[data-diagram="plantuml"]');
  if (hosts.length === 0) return;
  for (const host of hosts) host.removeAttribute('data-mermaid');
  if (document.body) void hydrateMermaid(document.body);
});

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

/* ------------------------------------------------------------------ *
 * Workspace-path interception (file-explorer drawer) + explicit
 * http(s) link download. Like the fence-copy handler above, both are
 * delegated document-level listeners, so every rendered markdown surface
 * is covered: host App, StreamingAssistant, and the collab guest view.
 * ------------------------------------------------------------------ */

/** A parsed workspace-path candidate: workspace-RELATIVE path plus the
 *  optional `:line[:col]` target parsed off it. */
export interface MarkdownPathCandidate {
  path: string;
  line?: number;
  column?: number;
}

/** Exact rendered owner for a resolved workspace-path click. */
export interface MarkdownPathOwner {
  sessionId: string;
  runnerId?: string;
  mappingId?: string;
  workspaceCwd?: string;
}

/** Receiver for workspace-relative path clicks and their rendered owner.
 *  `completion` is one longer unique same-context path, used only after the
 *  exact path is a typed missing file. */
export type MarkdownOpenPathHandler = (
  path: string,
  line: number | undefined,
  owner: MarkdownPathOwner,
  completion?: string,
) => void;

interface PathClickNodeLike {
  textContent: string | null;
  getAttribute(name: string): string | null;
  closest(selectors: string): unknown;
  dataset?: { mdDownloadBusy?: string };
}

const MAX_WORKSPACE_PATH_CHARS = 512;
const MAX_WORKSPACE_PATH_LINE = 10_000_000;
const MAX_WORKSPACE_PATH_COLUMN = 1_000_000;
const MAX_MARKDOWN_DOWNLOAD_BYTES = 16 * 1024 * 1024;
const MAX_DOWNLOAD_FILENAME_LENGTH = 128;
const WORKSPACE_FILE_LINE = /^\d+(:\d+)?$/;
const WORKSPACE_FILE_RANGE = /^\d+-\d+$/;
const BLOCKED_WORKSPACE_SCHEMES = /^(javascript|data|file|https?|mailto|vbscript|blob):/i;
const RFC_SCHEME_PREFIX = /^[A-Za-z][A-Za-z0-9+.-]*:/;

/** True when `text` starts with an RFC 3986 `scheme:` token. */
function hasRfcScheme(text: string): boolean {
  return RFC_SCHEME_PREFIX.test(text);
}
// `path[:line[:col]]` candidates tolerate exactly these trailing punctuation
// marks (sentence / parenthesis debris) and nothing else.
const WORKSPACE_PATH_TRAILING: Record<string, true> = {
  '.': true,
  ',': true,
  ')': true,
  ':': true,
  ';': true,
  '`': true,
};

/** One global workspace-open receiver: Strict Mode ref-count so a single
 *  lifecycle subscriber stays live across Strict Mode double-invoke. */
const markdownOpenPathHandlers = new Map<MarkdownOpenPathHandler, number>();

function clampInt(value: number, min: number, max: number): number {
  if (!Number.isFinite(value)) return min;
  if (value < min) return min;
  if (value > max) return max;
  return Math.floor(value);
}

/**
 * Workspace `path[:line[:col]]` / `path:start-end`. href rejects RFC `scheme:`;
 * code keeps `Makefile:12` / `file.ext:line` / `file.ext:start-end`.
 * Rejects `#`, `..`, whitespace, trailing `/` `\`; trims `. , ) : ; \``.
 * Href rejects a leading `/` `\`; code keeps workspace-absolute descendants.
 * Code-surface: separator, `:line`, known type, or 2+ char suffix (not `x.y`).
 */
export function parseWorkspacePathCandidate(
  text: string,
  surface: 'code' | 'href' = 'code',
): MarkdownPathCandidate | null {
  if (typeof text !== 'string') return null;
  let s = text.trim();
  if (!s) return null;
  if (surface === 'href' && hasRfcScheme(s)) return null;
  while (s.length > 0 && WORKSPACE_PATH_TRAILING[s.charAt(s.length - 1)] === true) {
    s = s.slice(0, -1);
  }
  s = s.trim();
  if (!s || s.length > MAX_WORKSPACE_PATH_CHARS) return null;
  if (s.endsWith('/') || s.endsWith('\\')) return null;
  if (/[\u0000-\u001f\u007f]/.test(s) || /\s/.test(s)) return null;
  if (surface !== 'href') {
    if (BLOCKED_WORKSPACE_SCHEMES.test(s)) return null;
    const scheme = s.match(/^([a-zA-Z][a-zA-Z0-9+.-]*):(.*)$/);
    if (scheme && !WORKSPACE_FILE_LINE.test(scheme[2]) && !WORKSPACE_FILE_RANGE.test(scheme[2])) return null;
  }
  let path = s;
  let line: number | undefined;
  let column: number | undefined;
  const withCol = s.match(/^(.*?):(\d+):(\d+)$/);
  const withLine = withCol ? null : s.match(/^(.*?):(\d+)$/);
  const withRange = withCol || withLine ? null : s.match(/^(.*?):(\d+)-(\d+)$/);
  if (withCol && withCol[1]) {
    path = withCol[1];
    line = clampInt(Number(withCol[2]), 1, MAX_WORKSPACE_PATH_LINE);
    column = clampInt(Number(withCol[3]), 1, MAX_WORKSPACE_PATH_COLUMN);
  } else if (withLine && withLine[1]) {
    path = withLine[1];
    line = clampInt(Number(withLine[2]), 1, MAX_WORKSPACE_PATH_LINE);
  } else if (withRange && withRange[1]) {
    path = withRange[1];
    line = clampInt(Number(withRange[2]), 1, MAX_WORKSPACE_PATH_LINE);
  }

  if (!path || path.length > MAX_WORKSPACE_PATH_CHARS) return null;
  if (path.charAt(0) === '#') return null;
  if (surface !== 'code' && (path.charAt(0) === '/' || path.charAt(0) === '\\')) return null;
  const segments = path.split(/[/\\]+/);
  if (segments.some((seg) => seg === '..')) return null;
  if (segments.every((seg) => seg === '' || seg === '.')) return null;
  if (surface === 'code' && !isCodeSurfacePathLike(path, line)) return null;

  const result: MarkdownPathCandidate = { path };
  if (line !== undefined) result.line = line;
  if (column !== undefined) result.column = column;
  return result;
}

function isCodeSurfacePathLike(path: string, line: number | undefined): boolean {
  if (line !== undefined) return true;
  if (path.includes('/') || path.includes('\\')) return true;
  const base = path.split(/[/\\]/).pop() ?? path;
  const dot = base.lastIndexOf('.');
  if (dot <= 0 || dot === base.length - 1) return false;
  const ext = base.slice(dot + 1);
  if (!/^[A-Za-z0-9]{1,10}$/.test(ext)) return false;
  if (codeLanguage(base) !== '') return true;
  return ext.length >= 2;
}

/**
 * Keep valid relative candidates byte-for-byte unchanged. Absolute candidates
 * cross the Markdown/Explorer boundary only after they are proven to be a
 * non-empty descendant of the rendered owner's authoritative workspace cwd.
 * Separator canonicalization is comparison-only for relative paths and forms
 * the returned `/`-separated path for absolute inputs.
 */
export function markdownWorkspaceRelativePath(
  candidate: string,
  workspaceCwd?: string | null,
): string | null {
  if (typeof candidate !== 'string' || candidate === '' || candidate.trim() !== candidate) return null;
  if (candidate.length > MAX_WORKSPACE_PATH_CHARS) return null;
  if (/[\u0000-\u001f\u007f]/.test(candidate) || /\s/.test(candidate)) return null;

  const candidateSegments = candidate.split(/[/\\]+/);
  if (candidateSegments.some((segment) => segment === '..')) return null;
  const driveAbsolute = /^[A-Za-z]:[/\\]/.test(candidate);
  const absolute = driveAbsolute || candidate.charAt(0) === '/' || candidate.charAt(0) === '\\';
  if (!absolute) {
    if (candidate.charAt(0) === '#' || hasRfcScheme(candidate)) return null;
    if (candidateSegments.every((segment) => segment === '' || segment === '.')) return null;
    return candidate;
  }

  const cwd = workspaceCwd?.trim();
  if (!cwd || cwd.length > MAX_WORKSPACE_PATH_CHARS) return null;
  if (/[\u0000-\u001f\u007f]/.test(cwd) || /\s/.test(cwd)) return null;
  const cwdDriveAbsolute = /^[A-Za-z]:[/\\]/.test(cwd);
  if (!cwdDriveAbsolute && cwd.charAt(0) !== '/' && cwd.charAt(0) !== '\\') return null;
  if (cwd.split(/[/\\]+/).some((segment) => segment === '..')) return null;

  const canonicalize = (path: string) => {
    const normalized = path.replace(/\\/g, '/').replace(/\/{2,}/g, '/');
    if (normalized === '/' || /^[A-Za-z]:\/$/.test(normalized)) return normalized;
    return normalized.replace(/\/$/, '');
  };
  const canonicalCandidate = canonicalize(candidate);
  const canonicalCwd = canonicalize(cwd);
  const candidateDrive = canonicalCandidate.match(/^([A-Za-z]):\//)?.[1];
  const cwdDrive = canonicalCwd.match(/^([A-Za-z]):\//)?.[1];
  if ((candidateDrive || cwdDrive) && candidateDrive?.toLowerCase() !== cwdDrive?.toLowerCase()) return null;
  const comparableCandidate = candidateDrive
    ? `${candidateDrive.toLowerCase()}${canonicalCandidate.slice(1)}`
    : canonicalCandidate;
  const comparableCwd = cwdDrive
    ? `${cwdDrive.toLowerCase()}${canonicalCwd.slice(1)}`
    : canonicalCwd;
  const cwdIsRoot = canonicalCwd === '/' || /^[A-Za-z]:\/$/.test(canonicalCwd);
  const prefix = cwdIsRoot ? comparableCwd : `${comparableCwd}/`;
  if (!comparableCandidate.startsWith(prefix)) return null;
  const relative = canonicalCandidate.slice(canonicalCwd.length + (cwdIsRoot ? 0 : 1));
  return relative || null;
}

function nearbyPathTokenBoundary(ch: string): boolean {
  return ch === '' || ch === '"' || ch === "'" || ch === '`' || ch.trim() === '';
}

function nearbyPathBasenameSuffixOk(after: string): boolean {
  if (after === '') return true;
  if (/^:\d+$/.test(after) || /^:\d+:\d+$/.test(after) || /^:\d+-\d+$/.test(after)) return true;
  if (/^:\d+:/.test(after) && !after.includes('/') && !after.includes('\\')) return true;
  if (/^-\d+$/.test(after) || /^-\d+-/.test(after)) return true;
  return false;
}

function nearbyPathExpandToken(
  rootText: string,
  sepIndex: number,
  basenameEnd: number,
): { start: number; end: number } | null {
  let start = sepIndex;
  while (start > 0 && !nearbyPathTokenBoundary(rootText.charAt(start - 1))) {
    start -= 1;
    if (basenameEnd - start > MAX_WORKSPACE_PATH_CHARS) return null;
  }
  let end = basenameEnd;
  while (end < rootText.length && !nearbyPathTokenBoundary(rootText.charAt(end))) {
    end += 1;
    if (end - start > MAX_WORKSPACE_PATH_CHARS) return null;
  }
  return { start, end };
}

function markdownNearbyPathResolution(
  suffix: string,
  rootText: string | null | undefined,
): MarkdownNearbyPathResolution {
  if (typeof suffix !== 'string' || suffix === '') return { kind: 'none' };
  const suffixCanonical = fileExplorerCanonicalPath(suffix);
  if (
    !suffixCanonical
    || suffixCanonical.startsWith('/')
    || suffixCanonical.split('/').some((segment) => segment === '..')
  ) return { kind: 'none' };
  if (typeof rootText !== 'string' || rootText === '') return { kind: 'none' };
  let completion: string | null = null;
  let completionCanonical = '';
  let from = 0;
  for (;;) {
    const hit = rootText.indexOf(suffix, from);
    if (hit < 0) break;
    from = hit + 1;
    if (hit > 0) {
      const before = rootText.charAt(hit - 1);
      if (before !== '/' && before !== '\\' && !nearbyPathTokenBoundary(before)) continue;
    }
    const span = nearbyPathExpandToken(rootText, hit, hit + suffix.length);
    if (!span) continue;
    let token = rootText.slice(span.start, span.end);
    while (token.length > 0 && WORKSPACE_PATH_TRAILING[token.charAt(token.length - 1)] === true) {
      token = token.slice(0, -1);
    }
    const suffixAt = hit - span.start;
    if (suffixAt < 0 || token.slice(suffixAt, suffixAt + suffix.length) !== suffix) continue;
    const after = token.slice(suffixAt + suffix.length);
    if (!nearbyPathBasenameSuffixOk(after)) continue;
    const pathToken = token.slice(0, suffixAt + suffix.length);
    const relative = markdownWorkspaceRelativePath(pathToken);
    if (!relative) continue;
    const canonical = fileExplorerCanonicalPath(relative);
    if (canonical === suffixCanonical || canonical.length <= suffixCanonical.length || !canonical.endsWith(`/${suffixCanonical}`)) continue;
    if (completion === null) {
      completion = relative;
      completionCanonical = canonical;
    } else if (completionCanonical !== canonical) {
      return { kind: 'ambiguous' };
    }
  }
  return completion === null ? { kind: 'none' } : { kind: 'unique', completion };
}

export function markdownNearbyPathCompletion(
  suffix: string,
  rootText: string | null | undefined,
): string | null {
  const resolution = markdownNearbyPathResolution(suffix, rootText);
  return resolution.kind === 'unique' ? resolution.completion : null;
}

/** Longer unique workspace-relative path ending in `path` on a segment
 *  boundary, or null. The cited path itself is never a completion, so an
 *  exact path is not replaced by a longer lookalike. Ambiguous suffixes,
 *  absolute paths, and `..` stay null. */
export function markdownCompletedWorkspacePath(
  path: string,
  rootText: string | null | undefined,
): string | null {
  if (typeof path !== 'string' || path === '') return null;
  return markdownNearbyPathCompletion(path, rootText);
}

/**
 * The workspace-relative path a successful file_explorer_read probe echo
 * actually resolves to, or `null` when the echo proves nothing about the
 * request. Directories, missing files, malformed payloads, and mismatched
 * responses resolve to `null`. For a single-segment basename request the
 * server's unique-basename fallback echoes the REAL deeper path — callers
 * must open THAT path (never the request basename) so pagination reads the
 * established identity instead of repeatedly suffix-matching homonyms.
 */
export function markdownPathReadResolvedPath(path: string, response: unknown): string | null {
  const parsed = normalizeRead(response);
  if (!parsed) return null;
  return fileExplorerResolvedRequestPath(path, parsed.path);
}

export function markdownPathReadValid(path: string, response: unknown): boolean {
  return markdownPathReadResolvedPath(path, response) !== null;
}

/** The client-side copy of the host's own missing-file verdict
 *  (`FileExplorerError::FileUnavailable`: `rpcCode: 'file_unavailable'`,
 *  message `file is unavailable`). A probe that was ANSWERED but resolved no
 *  file for the requested workspace path is exactly that verdict — the same
 *  shape `onResponse` stamps on the host's rejection — so every caller
 *  routes it through its existing file_unavailable channel (App's
 *  refresh+retry, then the named `文件已不存在：…` toast) instead of dropping
 *  a dead click. */
function markdownOpenFileUnavailableError(): Error & { rpc: true; rpcCode: 'file_unavailable' } {
  const error = new Error('file is unavailable') as Error & { rpc: true; rpcCode: 'file_unavailable' };
  error.rpc = true;
  error.rpcCode = 'file_unavailable';
  return error;
}

/**
 * Gate a workspace-path open behind an exact-owner file_explorer_read
 * probe (maxLines 1) before the drawer opens: the EXACT rendered owner
 * session must answer with a parseable read whose echoed path canonically
 * matches the requested path. A send REJECTION (transport drop, rpc
 * failure, anything else) is user-owned: `onError` is invoked exactly once
 * with the rejection and the caller routes it to its one existing error
 * channel (App toast / Chat form error) — never silently swallowed. An
 * ANSWERED probe (missing file, directory, malformed payload, mismatched
 * echo — the requested path resolved to no readable file) is equally
 * user-owned and reaches the same channel once as the typed
 * `file_unavailable` verdict: a silently dead click is never an outcome.
 * The absolute→relative workspace bound (markdownWorkspaceRelativePath)
 * still applies first: an invalid candidate errors nothing and opens
 * nothing.
 */
export async function markdownOpenValidatedPath(
  path: string,
  line: number | undefined,
  owner: MarkdownPathOwner,
  send: (command: Record<string, unknown>) => Promise<unknown>,
  open: (path: string, line: number | undefined, owner: MarkdownPathOwner) => void,
  onError: (error: unknown) => void,
  completion?: string,
): Promise<void> {
  const sessionId = owner.sessionId.trim();
  const relative = markdownWorkspaceRelativePath(path, owner.workspaceCwd);
  if (!sessionId || !relative || relative.trim() !== relative) return;
  const command = fileExplorerCommandForSession(sessionId, fileExplorerReadCommand(relative, undefined, 1));
  let response: unknown;
  try {
    response = await send(command);
  } catch (error) {
    const typedMissing = (error as { rpcCode?: unknown } | null)?.rpcCode === 'file_unavailable';
    const fallback = typedMissing ? markdownWorkspaceRelativePath(completion ?? '', owner.workspaceCwd) : null;
    if (!typedMissing || !fallback || fallback === relative) {
      onError(error);
      return;
    }
    try {
      response = await send(fileExplorerCommandForSession(sessionId, fileExplorerReadCommand(fallback, undefined, 1)));
    } catch (retryError) {
      onError(retryError);
      return;
    }
    const resolvedFallback = markdownPathReadResolvedPath(fallback, response);
    if (!resolvedFallback) {
      onError(markdownOpenFileUnavailableError());
      return;
    }
    open(resolvedFallback, line, owner);
    return;
  }
  const resolved = markdownPathReadResolvedPath(relative, response);
  if (!resolved) {
    onError(markdownOpenFileUnavailableError());
    return;
  }
  open(resolved, line, owner);
}

export const MARKDOWN_OPEN_MISSING_PATH_MAX_CHARS = 96;

export function markdownOpenBoundedPathText(path: string): string {
  const trimmed = path.trim();
  if (trimmed.length <= MARKDOWN_OPEN_MISSING_PATH_MAX_CHARS) return trimmed;
  return `…${trimmed.slice(-MARKDOWN_OPEN_MISSING_PATH_MAX_CHARS)}`;
}

export function markdownOpenMissingFileVerdict(retried: boolean, error: unknown): boolean {
  if (!retried || error === null || typeof error !== 'object') return false;
  return (error as { rpcCode?: unknown }).rpcCode === 'file_unavailable';
}

export function markdownOpenMissingFileToast(path: string): string {
  return `文件已不存在：${markdownOpenBoundedPathText(path)}`;
}

/** Register a workspace-path click receiver. Same function is ref-counted
 *  so App's one lifecycle subscriber stays live across Strict Mode
 *  double-invoke until the last unsubscriber. Returns an unsubscribe
 *  function; the handler is always delivered to (single-document behavior). */
export function subscribeMarkdownOpenPath(handler: MarkdownOpenPathHandler): () => void {
  const existing = markdownOpenPathHandlers.get(handler);
  markdownOpenPathHandlers.set(handler, (existing ?? 0) + 1);
  return () => {
    const entry = markdownOpenPathHandlers.get(handler);
    if (entry === undefined) return;
    if (entry <= 1) markdownOpenPathHandlers.delete(handler);
    else markdownOpenPathHandlers.set(handler, entry - 1);
  };
}

function asPathClickNode(value: unknown): PathClickNodeLike | null {
  if (!value || typeof value !== 'object') return null;
  const node = value as PathClickNodeLike;
  if (typeof node.getAttribute !== 'function' || typeof node.closest !== 'function') return null;
  return node;
}

/**
 * Deliver the cited workspace-relative path unchanged. A unique longer
 * same-context path is a separate completion argument, used only after a
 * typed missing-file probe. Absolute and out-of-workspace candidates stay
 * inert before either value is delivered.
 */
function notifyMarkdownOpenPath(
  candidate: MarkdownPathCandidate,
  owner: MarkdownPathOwner,
  extra?: MarkdownOpenPathHandler,
  rootText?: string | null,
  stampedCompletion?: string | null,
): boolean {
  const path = markdownWorkspaceRelativePath(candidate.path, owner.workspaceCwd);
  if (!path) return false;
  const originalAbsolute = candidate.path.charAt(0) === '/' || candidate.path.charAt(0) === '\\' || /^[A-Za-z]:[/\\]/.test(candidate.path);
  const fromStamp = !originalAbsolute && stampedCompletion
    ? markdownWorkspaceRelativePath(stampedCompletion, owner.workspaceCwd)
    : null;
  const discovered = originalAbsolute
    ? null
    : fromStamp && fromStamp !== path
      ? fromStamp
      : markdownCompletedWorkspacePath(path, fromStamp ? null : rootText);
  const completion = discovered && discovered !== path ? discovered : undefined;
  let called = false;
  if (extra) {
    extra(path, candidate.line, owner, completion);
    called = true;
  }
  for (const handler of markdownOpenPathHandlers.keys()) {
    if (handler === extra) continue;
    handler(path, candidate.line, owner, completion);
    called = true;
  }
  return called;
}

function flashDownloadFeedback(button: PathClickNodeLike, message: string): void {
  const original = button.textContent;
  button.textContent = message.slice(0, 48);
  if (typeof window === 'undefined') return;
  window.setTimeout(() => {
    button.textContent = original;
  }, 1600);
}

/** Basename + control-char scrub for an explicit markdown-link download.
 *  Mirrors sessionExportFileName bounds without the `.html` requirement. */
export function markdownDownloadFileName(href: string, contentDisposition?: string | null): string {
  let name = '';
  if (contentDisposition) {
    const encoded = contentDisposition.match(/filename\*\s*=\s*UTF-8''([^;]+)/i);
    const quoted = contentDisposition.match(/filename\s*=\s*"([^"]+)"/i);
    const bare = contentDisposition.match(/filename\s*=\s*([^;]+)/i);
    const raw = encoded?.[1] ?? quoted?.[1] ?? bare?.[1] ?? '';
    try {
      name = decodeURIComponent(raw).trim();
    } catch {
      name = raw.trim();
    }
  }
  if (!name) {
    try {
      const url = new URL(href);
      const base = url.pathname.split('/').filter(Boolean).pop() ?? '';
      name = decodeURIComponent(base);
    } catch {
      name = '';
    }
  }
  const bounded = name
    .replace(/[\u0000-\u001f\u007f\\/]+/g, '-')
    .trim()
    .slice(0, MAX_DOWNLOAD_FILENAME_LENGTH);
  return bounded || 'download';
}

async function downloadMarkdownHref(href: string, button: PathClickNodeLike): Promise<void> {
  if (!/^https?:\/\//i.test(href)) {
    flashDownloadFeedback(button, 'blocked');
    return;
  }
  if (button.dataset?.mdDownloadBusy === '1') {
    flashDownloadFeedback(button, 'busy');
    return;
  }
  if (!button.dataset) button.dataset = {};
  button.dataset.mdDownloadBusy = '1';
  try {
    const response = await fetch(href, { credentials: 'omit', referrerPolicy: 'no-referrer' });
    if (!response.ok) {
      flashDownloadFeedback(button, `failed — HTTP ${response.status}`);
      return;
    }
    const blob = await response.blob();
    if (blob.size > MAX_MARKDOWN_DOWNLOAD_BYTES) {
      flashDownloadFeedback(button, 'failed — too large');
      return;
    }
    const objectUrl = URL.createObjectURL(blob);
    try {
      const anchor = document.createElement('a');
      anchor.href = objectUrl;
      anchor.download = markdownDownloadFileName(href, response.headers.get('content-disposition'));
      anchor.hidden = true;
      document.body.appendChild(anchor);
      anchor.click();
      anchor.remove();
    } finally {
      URL.revokeObjectURL(objectUrl);
    }
  } catch {
    flashDownloadFeedback(button, 'failed — retry');
  } finally {
    if (button.dataset) delete button.dataset.mdDownloadBusy;
  }
}

/**
 * Dispatch a markdown click: `.md-link-download` → fetch+blob download;
 * `.md-code` (not inside a fence) → code path parse; a relative
 * `<a href>` → href path parse. Ordinary prose — including bare absolute
 * paths — is never interactive, so no auto-linked anchor exists to click.
 * Path handlers receive only a workspace-relative path plus the exact
 * rendered owner. Missing owners, absolute paths without an authoritative
 * cwd, and absolute paths outside that cwd stay inert. A bare-basename
 * candidate is completed from the SAME rendered owner's text when that text
 * names exactly one longer path ending in it (`gatherer.rs:178-260` cited
 * next to a `src/queue/gatherer.rs` grep hit probes the cited file).
 */
export function dispatchMarkdownPathClick(
  event: { target: unknown; preventDefault(): void; defaultPrevented?: boolean },
  onOpenPath?: MarkdownOpenPathHandler,
): boolean {
  if (event.defaultPrevented) return false;
  const target = asPathClickNode(event.target);
  if (!target) return false;
  const download = asPathClickNode(target.closest('.md-link-download'));
  if (download) {
    event.preventDefault();
    const href = (download.getAttribute('data-href') ?? '').trim();
    void downloadMarkdownHref(href, download);
    return true;
  }

  const ownerNode = asPathClickNode(target.closest('[data-session-id]'));
  const sessionId = ownerNode?.getAttribute('data-session-id')?.trim();
  if (!ownerNode || !sessionId) return false;
  const runnerId = ownerNode.getAttribute('data-runner-id')?.trim() || undefined;
  const mappingId = ownerNode.getAttribute('data-mapping-id')?.trim() || undefined;
  const workspaceCwd = ownerNode.getAttribute('data-workspace-cwd')?.trim() || undefined;
  const owner: MarkdownPathOwner = { sessionId };
  if (runnerId) owner.runnerId = runnerId;
  if (mappingId) owner.mappingId = mappingId;
  if (workspaceCwd) owner.workspaceCwd = workspaceCwd;

  const toolPath = asPathClickNode(target.closest('[data-md-path]'));
  if (toolPath) {
    const rawPath = (toolPath.getAttribute('data-md-path') ?? '').trim();
    const lineAttr = (toolPath.getAttribute('data-md-line') ?? '').trim();
    let line: number | undefined;
    if (lineAttr !== '') {
      const parsedLine = Number(lineAttr);
      if (Number.isFinite(parsedLine) && parsedLine >= 1 && parsedLine <= MAX_WORKSPACE_PATH_LINE) {
        line = Math.floor(parsedLine);
      }
    }
    const candidate: MarkdownPathCandidate = { path: rawPath };
    if (line !== undefined) candidate.line = line;
    const stamped = (toolPath.getAttribute('data-md-completion') ?? '').trim();
    if (!rawPath || !notifyMarkdownOpenPath(candidate, owner, onOpenPath, null, stamped)) return false;
    event.preventDefault();
    return true;
  }

  const code = asPathClickNode(target.closest('code.md-code') ?? target.closest('.md-code'));
  if (code && !code.closest('.md-fence')) {
    if ((code.getAttribute('data-md-resolved') ?? '') === 'none') return false;
    const candidate = parseWorkspacePathCandidate(code.textContent ?? '');
    if (!candidate) return false;
    const bare = !candidate.path.includes('/') && !candidate.path.includes('\\');
    if (bare && !code.getAttribute('data-md-path')) return false;
    if (!notifyMarkdownOpenPath(candidate, owner, onOpenPath, null, code.getAttribute('data-md-completion'))) return false;
    event.preventDefault();
    return true;
  }

  const anchor = asPathClickNode(target.closest('a'));
  if (!anchor) return false;
  const href = (anchor.getAttribute('href') ?? '').trim();
  const candidate = parseWorkspacePathCandidate(href, 'href');
  if (!candidate || !notifyMarkdownOpenPath(candidate, owner, onOpenPath, ownerNode.textContent)) return false;
  event.preventDefault();
  return true;
}

if (typeof document !== 'undefined') {
  document.addEventListener('click', (event) => {
    dispatchMarkdownPathClick(event);
  });
}
