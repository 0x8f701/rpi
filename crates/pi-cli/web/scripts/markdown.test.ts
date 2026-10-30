#!/usr/bin/env node
// Focused markdown fence-highlighting regression for src/markdown.ts —
// the renderFence function. Proves that Rust code blocks are fully
// highlighted (round-trip preserves source, tokens at beginning/middle/end
// get hljs spans), registered aliases normalize (rs/rust, rust,ignore),
// unknown languages and incomplete fences render safely, and XSS stays
// escaped. Run via `npm run build` (esbuild + node).
//
// Exit codes: 0 = every assertion held; 1 = a regression.
//
// Root cause reproduced: before the fix, a fence info string like
// ```rust,ignore was passed verbatim to hljs.getLanguage("rust,ignore"),
// which returns undefined. The renderer then fell back to highlightAuto,
// which calls _highlight(name, code, false) — ignoreIllegals:false — for
// every registered language. The Rust grammar declares `illegal: '</` and
// bails on many real-world snippets, so a wrong language (or plaintext)
// with only a few overlapping keyword spans wins the relevance race.
// Result: "only a small part is highlighted." The fix splits the info
// string to the base token (rust), resolves aliases via getLanguage, and
// calls hljs.highlight with ignoreIllegals:true so the grammar consumes
// the ENTIRE source, emitting every token — including late ones — as spans.
import {
  renderFence,
  renderMarkdown,
  renderStreamingMarkdown,
  renderBlocks,
  thinkingBodyInnerHtml,
  thinkingFirstBlocks,
  hydrateMermaid,
  mermaidRenderIdentity,
  mermaidSourcePalette,
  prepareMermaidSource,
  mermaidSvgCacheTest,
  diffPathLanguage,
  highlightDiffLineFragments,
  parseWorkspacePathCandidate,
  markdownWorkspaceRelativePath,
  markdownNearbyPathCompletion,
  markdownCompletedWorkspacePath,
  dispatchMarkdownPathClick,
  subscribeMarkdownOpenPath,
  markdownOpenValidatedPath,
  markdownPathReadValid,
  markdownOpenMissingFileVerdict,
  markdownOpenMissingFileToast,
  markdownDownloadFileName,
} from '../src/markdown.ts';
import {
  configurePlantumlServer,
  configuredPlantumlServer,
  encodePlantuml64,
  parsePlantumlServer,
  plantumlSvgRequestUrl,
} from '../src/plantumlEncode.ts';
import {
  ASCII_PRIORITY_TOKENS,
  PHONE_TABLE_VIEWPORT,
  asciiTokenLineCount,
  firstColumnKeepsCjkLabel,
  mdTableCellRule,
  mdTableFirstColumnRule,
  mdTableIsScrollWrap,
  phonePriorityColumnContentPx,
  shippedTableCellsKeepAsciiTokens,
  tableCellWrapPolicy,
} from '../src/markdownTableLayout.ts';
import { measurePhoneTableLineRects } from './markdownTableBrowser.ts';
import { safeText } from '../src/redact.ts';
import { markdownOpenRejectionOwnership } from '../src/transportError.ts';
// The mermaid module is aliased to scripts/__mocks__/mermaid.ts by the build
// command; its fakeDocument/tempWrappers let the leak regression observe the
// wrapper lifecycle the real mermaid 11.16.1 render reproduces.
import mermaidMock, { fakeDocument, tempWrappers } from 'mermaid';
import fs from 'node:fs';
import path from 'node:path';
import { deflateRawSync } from 'node:zlib';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

/** The typed verdict fields a probe rejection carries, read through `in`
 *  narrowing (never an unchecked cast): an absent field stays absent. */
function rejectionFields(error: unknown): { rpc?: unknown; rpcCode?: unknown } {
  const fields: { rpc?: unknown; rpcCode?: unknown } = {};
  if (error === null || typeof error !== 'object') return fields;
  if ('rpc' in error) fields.rpc = error.rpc;
  if ('rpcCode' in error) fields.rpcCode = error.rpcCode;
  return fields;
}

// --- helpers ---

/** Extract the <code> inner HTML from a renderFence result. */
function codeInner(html: string): string {
  const m = html.match(/<code class="md-code hljs">([\s\S]*?)<\/code>/);
  return m ? m[1] : '';
}

/** Strip HTML tags and unescape entities — the textContent of the <code>. */
function textContent(html: string): string {
  return codeInner(html)
    .replace(/<[^>]*>/g, '')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"')
    .replace(/&#x27;/g, "'")
    .replace(/&#39;/g, "'")
    .replace(/&amp;/g, '&');
}

/**
 * Check that a plain-ASCII token appears inside its own hljs span.
 * hljs wraps each keyword/literal individually: <span class="hljs-keyword">use</span>.
 */
function hasHljsSpan(html: string, token: string): boolean {
  const escaped = token.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return new RegExp(`<span class="hljs-[^"]*">${escaped}</span>`).test(html);
}

/**
 * textContent of a per-line highlight fragment (no <code> wrapper): strip
 * tags, unescape entities — the exact text the browser would report.
 */
function fragmentText(html: string): string {
  return html
    .replace(/<[^>]*>/g, '')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"')
    .replace(/&#x27;/g, "'")
    .replace(/&#39;/g, "'")
    .replace(/&amp;/g, '&');
}

function streamingSplits(
  name: string,
  fullText: string,
  finalCheck?: (html: string) => boolean,
): void {
  const final = renderMarkdown(fullText);
  for (let split = 0; split <= fullText.length; split += 1) {
    const prefix = fullText.slice(0, split);
    const html = renderStreamingMarkdown(prefix);
    check(`${name}: split ${split} has no script element`, !/<script\b/i.test(html), html);
    check(`${name}: split ${split} has no element event handler`, !/<[^>]+\son[a-z]+\s*=/i.test(html), html);
    check(`${name}: split ${split} has no javascript URL`, !/href=["']javascript\s*:/i.test(html), html);
    check(`${name}: split ${split} equals ordinary prefix render`, html === renderMarkdown(prefix), html);
  }
  check(`${name}: final converges exactly`, renderStreamingMarkdown(fullText) === final);
  if (finalCheck) check(`${name}: final structure`, finalCheck(final), final);
}

// ---- streaming Markdown: every arbitrary chunk boundary recomputes the
// unstable tail through the one ordinary parser/sanitizer. The full result
// must converge byte-for-byte to the non-streaming renderer. ----
streamingSplits(
  'stream emphasis',
  'before **strong text** and *emphasis* after',
  (html) => html.includes('<strong>strong text</strong>') && html.includes('<em>emphasis</em>'),
);
streamingSplits(
  'stream safe link',
  'visit [safe link](https://example.com/path?q=1) now',
  (html) => html.includes('<a href="https://example.com/path?q=1" rel="noopener noreferrer">safe link</a>'),
);
streamingSplits(
  'stream link safety',
  '[safe](https://example.com) [mail](mailto:test@example.com) [bad](javascript:alert(1))',
  (html) => html.includes('href="https://example.com"')
    && html.includes('href="mailto:test@example.com"')
    && !html.toLowerCase().includes('href="javascript:'),
);
streamingSplits(
  'stream inline code',
  'call `renderMarkdown(fullText)` safely',
  (html) => html.includes('<code class="md-code">renderMarkdown(fullText)</code>'),
);
streamingSplits(
  'stream fenced code',
  '```rust\nfn main() {\n    println!("<safe>");\n}\n```\nafter',
  (html) => html.includes('class="md-fence"') && html.includes('println!') && !html.includes('<safe>'),
);
streamingSplits('stream escapes', String.raw`escaped \\*literal\\* and \\` + '`code`');
streamingSplits('stream incomplete constructs', 'prefix **unfinished [link](https://example.com `code');
streamingSplits(
  'stream hostile html and urls',
  '<script>alert(1)</script> <img src=x onerror=alert(2)> [bad](javascript:alert(3)) '
    + '[encoded](JaVaScRiPt:alert(4)) `</code><svg onload=alert(5)>`',
  (html) => html.includes('&lt;script&gt;alert(1)&lt;/script&gt;')
    && html.includes('&lt;img src=x onerror=alert(2)&gt;')
    && !/<script\b|<img\b|<svg\b/i.test(html),
);
// --- representative Rust source: attributes, lifetimes, async fn, match,
//     macros, raw strings, generics, comments, XSS bait ---
const RUST_SOURCE = [
  '// Top-level line comment',
  '#![allow(dead_code)]',
  'use std::sync::Arc;',
  '',
  '/// Doc comment with <html> & special chars',
  '#[derive(Debug, Clone)]',
  "pub struct Parser<'a> {",
  "    source: &'a str,",
  "    tokens: Vec<Token<'a>>,",
  '}',
  '',
  "impl<'a> Parser<'a> {",
  '    pub async fn parse(&mut self) -> Result<(), Error> {',
  '        let raw = r#"raw "string" here"#;',
  '        println!("parsing: {}", raw);',
  '        match self.next() {',
  '            Some(t) => Ok(t),',
  '            None => Err(Error::Eof),',
  '        }',
  '    }',
  '}',
].join('\n');

// ---- Rust: full source round-trips (strip HTML → original source) ----
{
  const html = renderFence('rust', RUST_SOURCE);
  const text = textContent(html);
  check('rust round-trip: strip HTML equals source', text === RUST_SOURCE,
    `got ${text.length} chars, expected ${RUST_SOURCE.length}`);
}

// ---- Rust: tokens at beginning/middle/end receive hljs spans ----
{
  const code = codeInner(renderFence('rust', RUST_SOURCE));
  // Beginning: 'use' (line 3, near start)
  check('rust beginning token "use" has hljs span', hasHljsSpan(code, 'use'),
    code.slice(0, 120));
  // Middle: 'match' (line 16, mid-late)
  check('rust middle token "match" has hljs span', hasHljsSpan(code, 'match'),
    code.slice(Math.floor(code.length / 2) - 80, Math.floor(code.length / 2) + 80));
  // End: 'None' (line 18, near end)
  check('rust end token "None" has hljs span', hasHljsSpan(code, 'None'),
    code.slice(-120));
}

// ---- rs alias produces identical Rust highlighting (alias normalization) ----
{
  const htmlRs = renderFence('rs', RUST_SOURCE);
  const htmlRust = renderFence('rust', RUST_SOURCE);
  check('rs alias: identical highlighting to rust', codeInner(htmlRs) === codeInner(htmlRust),
    'rs and rust produce different code output');
  check('rs alias: round-trip', textContent(htmlRs) === RUST_SOURCE);
  check('rs alias: label normalizes to "rust"',
    /<span class="md-fence__lang">rust<\/span>/.test(htmlRs), htmlRs);
}

// ---- rust,ignore info string highlights as Rust (not auto-detection) ----
{
  const htmlIgn = renderFence('rust,ignore', RUST_SOURCE);
  const htmlRust = renderFence('rust', RUST_SOURCE);
  check('rust,ignore: identical highlighting to rust',
    codeInner(htmlIgn) === codeInner(htmlRust));
  check('rust,ignore: round-trip', textContent(htmlIgn) === RUST_SOURCE);
  check('rust,ignore: label normalizes to "rust"',
    /<span class="md-fence__lang">rust<\/span>/.test(htmlIgn), htmlIgn);
}

// ---- rs,no_run also normalizes ----
{
  const html = renderFence('rs,no_run', RUST_SOURCE);
  check('rs,no_run: rust highlighting', hasHljsSpan(codeInner(html), 'match'));
  check('rs,no_run: round-trip', textContent(html) === RUST_SOURCE);
  check('rs,no_run: label normalizes to "rust"',
    /<span class="md-fence__lang">rust<\/span>/.test(html), html);
}

// ---- Rust source containing '</' (triggers illegal: '</' in grammar) ----
// ignoreIllegals:true must keep the grammar scanning past it.
{
  const src = 'let x = a</b;\nfn main() {}\n';
  const html = renderFence('rust', src);
  check('rust with </: round-trip', textContent(html) === src);
  check('rust with </: highlighting continues past illegal',
    hasHljsSpan(codeInner(html), 'fn'));
}

// ---- unknown language: round-trip works, XSS escaped ----
{
  const source = 'let x = "<script>alert(1)</script>";\nconsole.log(x);';
  const html = renderFence('xyzzy', source);
  const code = codeInner(html);
  check('unknown lang: round-trip', textContent(html) === source);
  check('unknown lang: no raw <script> tag', !code.includes('<script>'), code);
  check('unknown lang: &lt;script&gt; present', code.includes('&lt;script&gt;'), code);
}

// ---- empty language (auto-detection): round-trip works ----
{
  const html = renderFence('', RUST_SOURCE);
  check('empty lang: round-trip', textContent(html) === RUST_SOURCE);
  check('empty lang: label shows "text"',
    /<span class="md-fence__lang">text<\/span>/.test(html));
}

// ---- incomplete fence (no closing ```) handled by extractFences ----
{
  const raw = '```rust\nfn main() {\n    println!("hello");\n';
  const html = renderMarkdown(raw);
  check('incomplete fence: rendered as fence', html.includes('md-fence'));
  check('incomplete fence: round-trip',
    textContent(html) === 'fn main() {\n    println!("hello");\n');
  check('incomplete fence: highlighting applied',
    hasHljsSpan(codeInner(html), 'fn'));
}

// ---- XSS: HTML special chars in Rust source are escaped ----
{
  const xssSource = 'let x = "<img src=x onerror=alert(1)>"; // <script>';
  const html = renderFence('rust', xssSource);
  const code = codeInner(html);
  check('XSS: no raw <img> tag', !code.includes('<img'), code);
  check('XSS: no raw <script> tag', !code.includes('<script'), code);
  check('XSS: &lt;img&gt; present', code.includes('&lt;img'), code);
  check('XSS: round-trip', textContent(html) === xssSource);
}

// ---- catch safety: renderFence never throws ----
{
  let threw = false;
  try {
    renderFence('rust', '\x00'.repeat(100000));
  } catch {
    threw = true;
  }
  check('catch safety: no throw on unusual input', !threw);
}

// ---- diffPathLanguage: registered subset + shared path mapping ----
{
  check('diff lang rust from path', diffPathLanguage('src/a.rs') === 'rust');
  check('diff lang typescript', diffPathLanguage('web/util.ts') === 'typescript');
  // 'tsx' resolves through the registered typescript grammar's alias.
  check('diff lang tsx via alias', diffPathLanguage('web/App.tsx') === 'tsx');
  check('diff lang javascript', diffPathLanguage('lib/util.js') === 'javascript');
  check('diff lang json', diffPathLanguage('data/config.json') === 'json');
  check('diff lang python', diffPathLanguage('tool/build.py') === 'python');
  check('diff lang go', diffPathLanguage('cmd/main.go') === 'go');
  check('diff lang java', diffPathLanguage('src/Main.java') === 'java');
  check('diff lang yaml', diffPathLanguage('ci/workflow.yaml') === 'yaml');
  check('diff lang cpp', diffPathLanguage('src/buffer.cpp') === 'cpp');
  check('diff lang sql', diffPathLanguage('mig/001.sql') === 'sql');
  check('diff lang markdown', diffPathLanguage('README.md') === 'markdown');
  check('diff lang bash alias', diffPathLanguage('bin/run.sh') === 'bash');
  check('diff lang toml', diffPathLanguage('config.toml') === 'toml');
  // .sol → local registered Solidity grammar; null leaves Explorer + Review plain.
  check('diff lang solidity', diffPathLanguage('contracts/Token.sol') === 'solidity');
  // Plain text resolves via the registered plaintext grammar's 'text' alias.
  check('diff lang txt -> plaintext alias', diffPathLanguage('notes.txt') === 'text');
  // Unknown/plain-text extensions map to no registered language -> plain.
  check('diff lang kotlin not registered -> null', diffPathLanguage('src/Foo.kt') === null);
  check('diff lang dockerfile not registered -> null', diffPathLanguage('Dockerfile') === null);
  check('diff lang makefile not registered -> null', diffPathLanguage('Makefile') === null);
  check('diff lang no mapping -> null', diffPathLanguage('weird.dat') === null);
}

// ---- highlightDiffLineFragments: batch spans + verbatim textContent ----
{
  const lines = [
    'fn root_helper() -> u32 {',
    '    let cards = items.filter(|i| i.kind == "toolCard");',
    '    value',
    '}',
  ];
  const fragments = highlightDiffLineFragments('rust', lines);
  check('batch: aligned fragments', fragments.length === 4 && fragments.every((f) => f !== null),
    JSON.stringify(fragments.map((f) => (f === null ? null : f.slice(0, 60)))));
  const balance = (html: string) => (html.match(/<span /g) || []).length - (html.match(/<\/span>/g) || []).length;
  check('batch: beginning token span', hasHljsSpan(fragments[0]!, 'fn'), fragments[0]!);
  check('batch: mid token span', hasHljsSpan(fragments[1]!, 'let'), fragments[1]!);
  check('batch: every fragment balanced', fragments.every((f) => balance(f!) === 0), JSON.stringify(fragments));
  check('batch: textContent verbatim', fragmentText(fragments.join('\n')) === lines.join('\n'));
}

// ---- highlightDiffLineFragments: null markers split runs, stay plain ----
{
  const fragments = highlightDiffLineFragments('rust', ['fn main() {}', null, 'let x = 1;']);
  check('runs: aligned with null marker', fragments.length === 3 && fragments[1] === null);
  check('runs: first run highlighted', hasHljsSpan(fragments[0]!, 'fn'), fragments[0]!);
  check('runs: second run highlighted separately', hasHljsSpan(fragments[2]!, 'let'), fragments[2]!);
  check('runs: run fragments verbatim', fragmentText(fragments[0]!) === 'fn main() {}' && fragmentText(fragments[2]!) === 'let x = 1;');
  check('runs: empty input', highlightDiffLineFragments('rust', []).length === 0);
}

// ---- highlightDiffLineFragments: multi-line token stays balanced per line ----
{
  const lines = ['let s = r#"line one', 'line two"#;'];
  const fragments = highlightDiffLineFragments('rust', lines);
  check('multiline: aligned', fragments.length === 2 && fragments.every((f) => f !== null));
  const balance = (html: string) => (html.match(/<span /g) || []).length - (html.match(/<\/span>/g) || []).length;
  check('multiline: each fragment balanced', balance(fragments[0]!) === 0 && balance(fragments[1]!) === 0,
    `${fragments[0]!} || ${fragments[1]!}`);
  check('multiline: textContent verbatim', fragmentText(fragments.join('\n')) === lines.join('\n'));
}

// ---- highlightDiffLineFragments: hostile input stays literal, no side effect ----
{
  const hostile = ['let x = "<script>alert(1)</script>";'];
  const fragments = highlightDiffLineFragments('rust', hostile);
  const code = fragments[0]!;
  check('hostile: no raw <script> tag', !code.includes('<script'), code);
  check('hostile: escaped literal decodes to source', fragmentText(code) === hostile[0], code);
  check('hostile: &lt;script&gt; present', code.includes('&lt;script&gt;'), code);
}

// ---- highlightDiffLineFragments: secret redaction matches safeText ----
// The pane's textContent contract is exactly what its plain renderer shows:
// credential-shaped content becomes [REDACTED] (highlighted AND plain), while
// ordinary source stays verbatim. The fragment must never carry raw secrets.
{
  const secretLine = 'let apiKey = "sk-1234567890abcdef";';
  const secretFrag = highlightDiffLineFragments('rust', [secretLine]);
  check('redaction: fragment textContent matches safeText', fragmentText(secretFrag[0]!) === safeText(secretLine), secretFrag[0]!);
  check('redaction: raw secret never reaches the fragment', !secretFrag[0]!.includes('sk-1234567890abcdef'), secretFrag[0]!);
  check('redaction: [REDACTED] marker present', secretFrag[0]!.includes('[REDACTED]'), secretFrag[0]!);

  const mixed = ['fn main() {}', 'let token = "ghp_1234567890abcdef";', 'let x = 1;'];
  const mixedFrags = highlightDiffLineFragments('rust', mixed);
  check(
    'redaction: every line matches its safeText form',
    mixedFrags.every((f, i) => f !== null && fragmentText(f) === safeText(mixed[i])),
    JSON.stringify(mixedFrags),
  );
  check('redaction: ordinary lines stay verbatim', fragmentText(mixedFrags[0]!) === 'fn main() {}', mixedFrags[0]!);
  check('redaction: secret line is redacted', fragmentText(mixedFrags[1]!) === safeText(mixed[1]), mixedFrags[1]!);
}

// ---- highlightDiffLineFragments: unknown language falls back plain ----
{
  const fragments = highlightDiffLineFragments('not-a-language', ['fn main() {}', 'let x = 1;']);
  check('unknown lang: no throw, plain escaped fragments', fragments.length === 2 && fragments.every((f) => f !== null));
  check('unknown lang: textContent verbatim', fragmentText(fragments.join('\n')) === 'fn main() {}\nlet x = 1;');
  check('unknown lang: no hljs spans', !fragments[0]!.includes('hljs-'), fragments[0]!);
}
// ---- Solidity: local grammar + shared .sol mapping (Explorer + Review) ----
const SOLIDITY_SOURCE = [
  '// SPDX-License-Identifier: MIT <author@example.com>',
  'pragma solidity ^0.8.24;',
  '',
  'import {IERC20} from "./IERC20.sol";',
  '',
  '/**',
  ' * @title Escrow with <html> & special chars in natspec',
  ' */',
  'abstract contract Base {',
  '    address payable public owner;',
  '',
  '    modifier onlyOwner() {',
  '        require(msg.sender == owner, "not owner <b>");',
  '        _;',
  '    }',
  '}',
  '',
  'contract Token is Base {',
  '    event Transfer(address indexed from, address indexed to, uint256 value);',
  '    error Insufficient(uint256 requested, uint256 available);',
  '',
  '    mapping(address => uint256) private balances;',
  '    uint256 public constant TOTAL = 1_000_000e18; // 1M in wei',
  '    bytes32 public constant SALT = 0xdeadbeefcafe;',
  '    uint64 public constant LOCK = 2 days;',
  '    bytes public data = hex"deadbeef";',
  '    string public symbol = unicode"π";',
  '',
  '    constructor() {',
  '        owner = payable(msg.sender);',
  '    }',
  '',
  '    function transfer(address to, uint256 amount)',
  '        public onlyOwner returns (bool)',
  '    {',
  '        require(balances[msg.sender] >= amount, "insufficient");',
  '        balances[msg.sender] -= amount;',
  '        emit Transfer(msg.sender, to, amount);',
  '        return true;',
  '    }',
  '',
  '    function payload() external pure returns (string memory) {',
  '        return "<script>alert(1)</script>";',
  '    }',
  '}',
].join('\n');

{
  const html = renderFence('solidity', SOLIDITY_SOURCE);
  const code = codeInner(html);
  check('solidity: source intact across all lines', textContent(html) === SOLIDITY_SOURCE,
    `got ${textContent(html).length} chars, expected ${SOLIDITY_SOURCE.length}`);
  check('solidity: fence label normalizes to solidity',
    /<span class="md-fence__lang">solidity<\/span>/.test(html), html.slice(0, 160));
  check('solidity: pragma header is one meta span',
    code.includes('<span class="hljs-meta">pragma solidity ^0.8.24;</span>'), code.slice(0, 160));
  check('solidity keyword: contract', hasHljsSpan(code, 'contract'), code.slice(0, 120));
  check('solidity keyword: mapping', hasHljsSpan(code, 'mapping'));
  check('solidity keyword: require', hasHljsSpan(code, 'require'));
  check('solidity keyword: returns', hasHljsSpan(code, 'returns'));
  check('solidity keyword: emit', hasHljsSpan(code, 'emit'));
  check('solidity keyword: indexed', hasHljsSpan(code, 'indexed'));
  check('solidity: function name title span', hasHljsSpan(code, 'transfer'));
  check('solidity: modifier name title span', hasHljsSpan(code, 'onlyOwner'));
  check('solidity type: uint256', hasHljsSpan(code, 'uint256'));
  check('solidity type: address', hasHljsSpan(code, 'address'));
  check('solidity type: bytes32', hasHljsSpan(code, 'bytes32'));
  check('solidity type: bool', hasHljsSpan(code, 'bool'));
  check('solidity built_in: msg', hasHljsSpan(code, 'msg'));
  check('solidity literal: days unit', hasHljsSpan(code, 'days'));
  check('solidity number: separators+exponent', hasHljsSpan(code, '1_000_000e18'));
  check('solidity number: hex literal', hasHljsSpan(code, '0xdeadbeefcafe'));
  check('solidity: hex string literal span', code.includes('<span class="hljs-string">hex&quot;deadbeef&quot;</span>'));
  check('solidity: unicode string literal span', code.includes('<span class="hljs-string">unicode&quot;π&quot;</span>'));
  check('solidity: string XSS stays escaped', !code.includes('<script') && code.includes('&lt;script&gt;'));
  check('solidity: natspec html stays escaped', !code.includes('<html') && code.includes('&lt;html&gt;'));
  check('solidity: line-comment angle brackets escaped',
    !code.includes('<author@example.com>') && code.includes('&lt;author@example.com&gt;'));
  check('solidity: comment spans present', code.includes('<span class="hljs-comment">'));
  check('solidity: natspec doctag span', hasHljsSpan(code, '@title'));
  const htmlSol = renderFence('sol', SOLIDITY_SOURCE);
  check('solidity: sol alias identical output', codeInner(htmlSol) === code);
  check('solidity: sol alias label solidity',
    /<span class="md-fence__lang">solidity<\/span>/.test(htmlSol));
}

// ---- Solidity review-diff fragments (shared code-review pane path) ----
{
  const lines = [
    'pragma solidity ^0.8.24;',
    'contract Token is Base {',
    '    function transfer(address to, uint256 amount) public returns (bool) {',
    '        emit Transfer(msg.sender, to, amount);',
    '    }',
  ];
  const fragments = highlightDiffLineFragments('solidity', lines);
  check('solidity diff: aligned fragments', fragments.length === 5 && fragments.every((f) => f !== null),
    JSON.stringify(fragments));
  check('solidity diff: pragma meta span', fragments[0]!.includes('class="hljs-meta"'), fragments[0]!);
  check('solidity diff: contract keyword + name span',
    hasHljsSpan(fragments[1]!, 'contract') && hasHljsSpan(fragments[1]!, 'Token'), fragments[1]!);
  check('solidity diff: function name + returns spans',
    hasHljsSpan(fragments[2]!, 'transfer') && hasHljsSpan(fragments[2]!, 'returns'), fragments[2]!);
  check('solidity diff: built_in msg span', hasHljsSpan(fragments[3]!, 'msg'), fragments[3]!);
  check('solidity diff: every fragment balanced',
    fragments.every((f) => (f!.match(/<span /g) || []).length === (f!.match(/<\/span>/g) || []).length), JSON.stringify(fragments));
  check('solidity diff: textContent verbatim', fragmentText(fragments.join('\n')) === lines.join('\n'));
}

// ---- Solidity: multi-line block comment stays balanced across diff lines ----
{
  const lines = ['/* begins <html>', ' * ends & more */ contract C {}'];
  const fragments = highlightDiffLineFragments('solidity', lines);
  check('solidity multiline comment: aligned', fragments.length === 2 && fragments.every((f) => f !== null));
  check('solidity multiline comment: balanced per line',
    (fragments[0]!.match(/<span /g) || []).length === (fragments[0]!.match(/<\/span>/g) || []).length
      && (fragments[1]!.match(/<span /g) || []).length === (fragments[1]!.match(/<\/span>/g) || []).length,
    `${fragments[0]!} || ${fragments[1]!}`);
  check('solidity multiline comment: comment spans on both lines',
    fragments[0]!.includes('class="hljs-comment"') && fragments[1]!.includes('class="hljs-comment"'));
  check('solidity multiline comment: textContent verbatim',
    fragmentText(fragments.join('\n')) === lines.join('\n'));
  check('solidity multiline comment: no raw html tag', !fragments.join('').includes('<html'));
  check('solidity multiline comment: contract after comment spans', hasHljsSpan(fragments[1]!, 'contract'), fragments[1]!);
}

// ---- Solidity: hostile diff line stays literal ----
{
  const hostile = ['string memory evil = "</span><script>alert(1)</script>";'];
  const fragments = highlightDiffLineFragments('solidity', hostile);
  check('solidity diff hostile: no raw script tag', !fragments[0]!.includes('<script'), fragments[0]!);
  check('solidity diff hostile: decodes to source', fragmentText(fragments[0]!) === hostile[0], fragments[0]!);
}
// Regression: an escaped quote must not terminate a unicode string early.
{
  const src2 = [
    'string public s = unicode"a\\"b";',
    'uint256 public after = 1;',
  ].join('\n');
  const html2 = renderFence('solidity', src2);
  const code2 = codeInner(html2);
  check('solidity: escaped quote keeps one string span on the line',
    (code2.match(/<span class="hljs-string">/g) || []).length === 1, code2.slice(0, 200));
  check('solidity: following declaration still spans after escaped quote', hasHljsSpan(code2, 'uint256'), code2.slice(-120));
  check('solidity: following declaration keyword still spans', hasHljsSpan(code2, 'public'));
  check('solidity: escaped-quote lines round-trip', textContent(html2) === src2);
}

// ---- mermaid fence still renders as host div ----
{
  const html = renderFence('mermaid', 'graph TD\n  A --> B');
  check('mermaid: host div (not fence)',
    html.includes('md-mermaid-host') && !html.includes('md-fence'));
}

{
  // Observed real model output: the whole fence indented by one space.
  // CommonMark tolerates 0-3 spaces on opener AND closer; without tolerance
  // the diagram renders as raw prose.
  const mermaid = renderMarkdown('briefing:\n\n ```mermaid\n flowchart LR\n   A --> B\n ```\n\ntrailing');
  check('indented mermaid fence renders a mermaid host',
    mermaid.includes('md-mermaid-host') && mermaid.includes('flowchart LR') && !mermaid.includes('```'), mermaid.slice(0, 300));
  const code = renderMarkdown('note:\n\n ```rust\n fn main() {}\n ```\n');
  check('indented plain fence renders a code fence',
    code.includes('md-fence') && !code.includes('```'), code.slice(0, 300));
  const fourSpace = renderMarkdown('text:\n\n     ```rust\n     fn f() {}\n     ```\n');
  check('4-space indent stays a plain code block (CommonMark boundary)',
    fourSpace.includes('```rust'), fourSpace.slice(0, 200));
}

{
  const html = renderFence('plantuml', 'Bob -> Alice : hello');
  check('plantuml: host div with diagram attr',
    html.includes('md-mermaid-host') && html.includes('data-diagram="plantuml"') && html.includes('Bob -&gt; Alice'));
  const puml = renderFence('puml', 'A -> B');
  check('puml alias: same plantuml host', puml.includes('data-diagram="plantuml"'));
}

{
  const raw = deflateRawSync('Bob -> Alice : hello');
  check('plantuml encode64 known vector',
    encodePlantuml64(raw) === 'SyfFKj2rKt3CoKnELR1Io4ZDoSa70000',
    encodePlantuml64(raw));
  check('plantuml encode64 zero triplet', encodePlantuml64(Uint8Array.of(0, 0, 0)) === '0000');
  check('plantuml server empty is off', parsePlantumlServer('') === '');
  check('plantuml server https accepted', parsePlantumlServer('https://plantuml.example/uml') === 'https://plantuml.example/uml');
  check('plantuml server rejects ftp', parsePlantumlServer('ftp://x') === null);
  check('plantuml server rejects credentials', parsePlantumlServer('https://user:pass@example.com/uml') === null);
  check('plantuml svg url has no encoding prefix',
    plantumlSvgRequestUrl('https://plantuml.example/uml', 'SyfFKj2rKt3CoKnELR1Io4ZDoSa70000')
      === 'https://plantuml.example/uml/svg/SyfFKj2rKt3CoKnELR1Io4ZDoSa70000');
}

// ---- uppercase info string normalizes ----
{
  const html = renderFence('Rust', RUST_SOURCE);
  check('Rust (uppercase): rust highlighting', hasHljsSpan(codeInner(html), 'use'));
  check('Rust (uppercase): round-trip', textContent(html) === RUST_SOURCE);
}

// ---- hydrateMermaid post-mutation callback ----
// The transcript pin controller re-pins in the same task as each async host
// mutation: the callback fires synchronously after every content replacement
// and never for an already-claimed host or a no-op. Mermaid is aliased to a
// deterministic stub by the Web build command.

mermaidSvgCacheTest.reset();


// ---------------------------------------------------------------------
// Minimal SVG DOMParser shim (mermaidViewer.test.ts convention): the
// sanitize/instantiate pipeline parses mermaid.render output through
// DOMParser and FAILS CLOSED when there is none, so every intended
// success path needs this shim under plain node. There is deliberately
// no innerHTML parsing fallback in production and none here. '&nbsp;'
// is the parse-failure sentinel (an undefined entity in XML):
// parseFromString returns a parsererror document, mirroring the browser.
// ---------------------------------------------------------------------
type SvgKind = 'element' | 'text' | 'comment';
interface SvgNode {
  kind: SvgKind;
  name: string;
  attrs: Map<string, string>;
  children: SvgNode[];
  parent: SvgNode | null;
  text: string;
}
const svgParseAttrs = (raw: string): Map<string, string> => {
  const attrs = new Map<string, string>();
  const re = /([^\s=]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'))?/g;
  for (let match = re.exec(raw); match; match = re.exec(raw)) {
    attrs.set(match[1], match[2] ?? match[3] ?? '');
  }
  return attrs;
};
const svgParse = (input: string): SvgNode => {
  const root: SvgNode = { kind: 'element', name: '#root', attrs: new Map(), children: [], parent: null, text: '' };
  const stack: SvgNode[] = [root];
  let i = 0;
  while (i < input.length) {
    if (input.startsWith('<!--', i)) {
      const end = input.indexOf('-->', i + 4);
      const text = end < 0 ? input.slice(i + 4) : input.slice(i + 4, end);
      stack[stack.length - 1].children.push({ kind: 'comment', name: '#comment', attrs: new Map(), children: [], parent: stack[stack.length - 1], text });
      i = end < 0 ? input.length : end + 3;
      continue;
    }
    if (input.charAt(i) === '<') {
      const gt = input.indexOf('>', i + 1);
      if (gt < 0) break;
      const body = input.slice(i + 1, gt).trim();
      i = gt + 1;
      if (body.startsWith('/')) {
        if (stack.length > 1) stack.pop();
        continue;
      }
      const selfClose = body.endsWith('/');
      const open = selfClose ? body.slice(0, -1).trim() : body;
      const sp = open.search(/\s/);
      const node: SvgNode = {
        kind: 'element',
        name: (sp < 0 ? open : open.slice(0, sp)).toLowerCase(),
        attrs: svgParseAttrs(sp < 0 ? '' : open.slice(sp)),
        children: [],
        parent: stack[stack.length - 1],
        text: '',
      };
      stack[stack.length - 1].children.push(node);
      if (!selfClose) stack.push(node);
      continue;
    }
    const next = input.indexOf('<', i);
    const text = input.slice(i, next < 0 ? input.length : next);
    i = next < 0 ? input.length : next;
    if (text !== '') {
      stack[stack.length - 1].children.push({ kind: 'text', name: '#text', attrs: new Map(), children: [], parent: stack[stack.length - 1], text });
    }
  }
  return root.children.find((n) => n.kind === 'element' && n.name === 'svg') ?? root;
};
const svgSerialize = (node: SvgNode): string => {
  if (node.kind === 'text') return node.text;
  if (node.kind === 'comment') return `<!--${node.text}-->`;
  const attr = [...node.attrs.entries()].map(([k, v]) => ` ${k}="${v}"`).join('');
  const inner = node.children.map(svgSerialize).join('');
  if (node.name === '#root') return inner;
  return `<${node.name}${attr}>${inner}</${node.name}>`;
};
const svgWrapElement = (node: SvgNode): Element => {
  const attrList = (): Attr[] => [...node.attrs.entries()].map(([name, value]) => ({
    name,
    localName: name.includes(':') ? name.slice(name.indexOf(':') + 1) : name,
    value,
    namespaceURI: name.startsWith('xlink:') ? 'http://www.w3.org/1999/xlink' : null,
  } as Attr));
  const descendants = (current: SvgNode): SvgNode[] => {
    const out: SvgNode[] = [];
    for (const child of current.children) {
      if (child.kind !== 'element') continue;
      out.push(child, ...descendants(child));
    }
    return out;
  };
  const element = {
    get localName() { return node.name; },
    get tagName() { return node.name; },
    get nodeName() { return node.name; },
    get outerHTML() { return svgSerialize(node); },
    get textContent() {
      if (node.kind !== 'element') return node.text;
      return node.children.map((child) => (
        child.kind === 'element' ? svgWrapElement(child).textContent : child.text
      )).join('');
    },
    set textContent(value: string) {
      node.children = [{ kind: 'text', name: '#text', attrs: new Map(), children: [], parent: node, text: String(value) }];
    },
    attributes: {
      get length() { return node.attrs.size; },
      item(index: number) { return attrList()[index] ?? null; },
    },
    getAttribute(name: string) { return node.attrs.get(name) ?? null; },
    setAttribute(name: string, value: string) { node.attrs.set(name, value); },
    setAttributeNS(_ns: string | null, name: string, value: string) { node.attrs.set(name, value); },
    removeAttribute(name: string) { node.attrs.delete(name); },
    remove() {
      const parent = node.parent;
      if (!parent) return;
      const index = parent.children.indexOf(node);
      if (index >= 0) parent.children.splice(index, 1);
      node.parent = null;
    },
    querySelector(selector: string) {
      if (selector === 'svg' && node.name === 'svg') return element;
      const hit = descendants(node).find((child) => child.name === selector);
      return hit ? svgWrapElement(hit) : null;
    },
    querySelectorAll(selector: string) {
      const all = descendants(node);
      const matched = selector === '*' ? all : all.filter((child) => child.name === selector);
      return matched.map(svgWrapElement);
    },
  };
  return element as unknown as Element;
};
class SvgDomParser {
  parseFromString(source: string, _type: string) {
    if (source.includes('&nbsp;')) {
      return { documentElement: { nodeName: 'parsererror', localName: 'parsererror', tagName: 'parsererror' } };
    }
    return { documentElement: svgWrapElement(svgParse(source)) };
  }
}
const prevDomParser = Reflect.get(globalThis, 'DOMParser');
Object.defineProperty(globalThis, 'DOMParser', { value: SvgDomParser, configurable: true, writable: true });

interface FakeHost {
  attrs: Map<string, string>;
  textContent: string;
  innerHTML: string;
  classes: Set<string>;
  isConnected: boolean;
  getAttribute(name: string): string | null;
  setAttribute(name: string, value: string): void;
  removeAttribute(name: string): void;
  classList: { add(cls: string): void };
}

function fakeHost(source: string, initial?: Record<string, string>): FakeHost {
  const attrs = new Map<string, string>(Object.entries(initial ?? {}));
  const classes = new Set<string>();
  return {
    attrs,
    textContent: source,
    innerHTML: source,
    classes,
    isConnected: true,
    getAttribute: (name: string) => attrs.get(name) ?? null,
    setAttribute: (name: string, value: string) => { attrs.set(name, value); },
    removeAttribute: (name: string) => { attrs.delete(name); },
    classList: { add: (cls: string) => { classes.add(cls); } },
  };
}

function fakeRoot(hosts: FakeHost[]): HTMLElement {
  return { isConnected: true, querySelectorAll: () => hosts } as unknown as HTMLElement;
}

{
  mermaidSvgCacheTest.reset();
  configurePlantumlServer('');
  check('plantuml server default is off', configuredPlantumlServer() === '');
  const host = fakeHost('Bob -> Alice : hello', { 'data-diagram': 'plantuml' });
  await hydrateMermaid(fakeRoot([host]));
  check('plantuml unset: fence hint',
    host.innerHTML.includes('md-fence--plantuml')
      && host.innerHTML.includes('配置 PlantUML server 后可渲染')
      && host.innerHTML.includes('Bob -&gt; Alice : hello'),
    host.innerHTML);
}

{
  mermaidSvgCacheTest.reset();
  configurePlantumlServer('https://plantuml.example/uml');
  const host = fakeHost('@startuml\nBob -> Alice : hello\n@enduml', { 'data-diagram': 'plantuml' });
  const prevFetch = globalThis.fetch;
  let fetched = '';
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    fetched = String(input);
    return new Response(
      '<svg class="mock-svg" width="100%" style="max-width: 640px" viewBox="0 0 640 120"></svg>',
      { status: 200, headers: { 'content-type': 'image/svg+xml' } },
    );
  }) as typeof fetch;
  try {
    await hydrateMermaid(fakeRoot([host]));
    check('plantuml configured: fetches /svg without encoding prefix',
      fetched.startsWith('https://plantuml.example/uml/svg/')
        && !fetched.includes('/svg/~1'),
      fetched);
    check('plantuml configured: same mermaid host toolbar',
      host.innerHTML.includes('md-mermaid-toolbar') && host.innerHTML.includes('data-mermaid-action="view"'),
      host.innerHTML);
  } finally {
    globalThis.fetch = prevFetch;
    configurePlantumlServer('');
  }
}

{
  // success path: rendered-SVG replacement fires the callback once
  const calls: string[] = [];
  const host = fakeHost('graph TD\n  A --> B');
  await hydrateMermaid(fakeRoot([host]), () => calls.push('mutated'));
  check('hydrate: svg replacement fired the callback once', calls.length === 1, `calls=${calls.length}`);
  check('hydrate: host holds the rendered svg', host.innerHTML.includes('<svg'), host.innerHTML);
  check('hydrate: success has no visible Mermaid label', !host.innerHTML.includes('Mermaid ·'), host.innerHTML);
  check('hydrate: success has no diagram-kind label', !host.innerHTML.includes('flowchart') && !host.innerHTML.includes('sequenceDiagram'), host.innerHTML);
  check('hydrate: rendered class added', host.classes.has('md-mermaid-host--rendered'));
  check('hydrate: host claimed done', host.getAttribute('data-mermaid') === 'done');
  check('hydrate: no error class', !host.classes.has('md-mermaid-host--error'));
}

{
  // Viewer toolbar: every successfully hydrated host carries the
  // always-visible View / Download SVG row (mermaidViewer.ts markup), and
  // the SVG renders inside its own horizontal scroller row BELOW the
  // toolbar — a wide diagram scrolls there, never the toolbar away.
  const host = fakeHost('graph TD\n  A --> B');
  await hydrateMermaid(fakeRoot([host]), () => { /* success path */ });
  check('viewer-toolbar: rendered host carries the toolbar row',
    host.innerHTML.includes('class="md-mermaid-toolbar"'), host.innerHTML);
  check('viewer-toolbar: View button present',
    host.innerHTML.includes('data-mermaid-action="view"') && host.innerHTML.includes('>View<'),
    host.innerHTML);
  check('viewer-toolbar: Download SVG button present',
    host.innerHTML.includes('data-mermaid-action="download"') && host.innerHTML.includes('>Download SVG<'),
    host.innerHTML);
  check('viewer-toolbar: toolbar row precedes the svg viewport row',
    host.innerHTML.includes('class="md-mermaid-viewport"')
      && host.innerHTML.indexOf('md-mermaid-toolbar') < host.innerHTML.indexOf('<svg'),
    host.innerHTML);
}

{
  const themed = 'flowchart LR\nA --> B';
  const classDefSource = 'flowchart LR\nA --> B\nclassDef foo fill:#f9f,stroke:#333\nclass A foo';
  const styleSource = 'flowchart LR\nA --> B\nstyle A fill:#fff,stroke:#333';
  const initSource = '%%{init: {"theme":"base","themeVariables":{"primaryColor":"#fff"}}}%%\nflowchart LR\nA --> B';
  check('palette: unmarked flowchart stays on the render theme', mermaidSourcePalette(themed) === 'theme');
  check('palette: classDef is author color', mermaidSourcePalette(classDefSource) === 'author');
  check('palette: style directive is author color', mermaidSourcePalette(styleSource) === 'author');
  check('palette: init directive is author color', mermaidSourcePalette(initSource) === 'author');
  const themeId = mermaidRenderIdentity('theme');
  const authorId = mermaidRenderIdentity('author');
  check('cache identity: theme and author keys differ', themeId !== authorId, `${themeId} vs ${authorId}`);
  check(
    'cache identity: theme key includes dark themeVariables JSON',
    themeId.includes('"primaryColor":"#151820"') && themeId.includes('\0dark\0'),
    themeId,
  );
  check(
    'cache identity: author key is default theme with empty themeVariables',
    authorId.includes('\0default\0') && authorId.endsWith('\0{}'),
    authorId,
  );
  check(
    'cache identity: same source plus palette is not a shared key',
    themeId + '\0' + themed !== authorId + '\0' + classDefSource,
  );
}

{
  const broken = 'flowchart TD\nB2["chain_domain / checkpoint_id"<br/>/ realm_sub_id"]';
  const prepared = prepareMermaidSource(broken);
  check(
    'quoted label plus br joins into one mermaid node label',
    prepared.includes('B2["chain_domain / checkpoint_id<br>/ realm_sub_id"]')
      && !prepared.includes('"<br/>'),
    prepared,
  );
  check(
    'curly quotes inside mermaid source become ASCII apostrophes',
    prepareMermaidSource('flowchart TD\nA["\u201cbound\u201d"]') === 'flowchart TD\nA["\'bound\'"]',
  );
}

{
  const themed = 'flowchart LR\nA --> B';
  const classDefSource = 'flowchart LR\nA --> B\nclassDef foo fill:#f9f,stroke:#333\nclass A foo';
  mermaidSvgCacheTest.reset();
  const themeHost = fakeHost(themed);
  const authorHost = fakeHost(classDefSource);
  await hydrateMermaid(fakeRoot([themeHost]));
  await hydrateMermaid(fakeRoot([authorHost]));
  check('hydrate: theme path does not pad a white canvas',
    !themeHost.innerHTML.includes(`style="background:#ffffff"`)
      && themeHost.getAttribute('data-mermaid-palette') !== 'author',
    themeHost.innerHTML);
  check('hydrate: author path pads a white canvas and stamps palette',
    authorHost.innerHTML.includes(`style="background:#ffffff"`)
      && authorHost.getAttribute('data-mermaid-palette') === 'author'
      && authorHost.classes.has('md-mermaid-host--rendered'),
    authorHost.innerHTML);
  check('cache: theme and author diagrams occupy distinct entries',
    mermaidSvgCacheTest.size() === 2, `size=${mermaidSvgCacheTest.size()}`);
  mermaidSvgCacheTest.reset();
}

{
  // Error and input-fallback hosts keep the degraded surface: no toolbar,
  // no viewport, no viewer wiring.
  const errorHost = fakeHost('BROKEN diagram');
  await hydrateMermaid(fakeRoot([errorHost]));
  check('viewer-toolbar: error host has no toolbar or viewport',
    !errorHost.innerHTML.includes('md-mermaid-toolbar')
      && !errorHost.innerHTML.includes('md-mermaid-viewport'),
    errorHost.innerHTML);
  const oversized = 'flowchart TD\n' + 'A --> B\n'.repeat(10_000);
  const fallback = renderFence('mermaid', oversized);
  check('viewer-toolbar: oversize input fallback host has no toolbar',
    !fallback.includes('md-mermaid-toolbar'), fallback.slice(0, 200));
}

{
  // error path: degradation mutation fires the callback once
  const calls: string[] = [];
  const host = fakeHost('BROKEN diagram');
  await hydrateMermaid(fakeRoot([host]), () => calls.push('mutated'));
  check('hydrate: error block fired the callback once', calls.length === 1, `calls=${calls.length}`);
  check('hydrate: error block rendered', host.innerHTML.includes('md-mermaid-error'), host.innerHTML);
  check('hydrate: error keeps source-fallback label', host.innerHTML.includes('Mermaid · source fallback'), host.innerHTML);
  check('hydrate: error keeps safe render reason', host.innerHTML.includes('Could not render:'), host.innerHTML);
  check('hydrate: error class added', host.classes.has('md-mermaid-host--error'));
}

{
  // empty host: clear mutation fires the callback, no render attempt
  const calls: string[] = [];
  const host = fakeHost('   \n  ');
  await hydrateMermaid(fakeRoot([host]), () => calls.push('mutated'));
  check('hydrate: empty host fired the callback once', calls.length === 1, `calls=${calls.length}`);
  check('hydrate: empty host cleared', host.innerHTML === '', host.innerHTML);
  check('hydrate: empty host got no render/error class', host.classes.size === 0);
}

{
  // already-hydrated host (data-mermaid="done"): skipped, no callback
  const calls: string[] = [];
  const source = 'graph TD\n  A --> B';
  const host = fakeHost(source);
  host.setAttribute('data-mermaid', 'done');
  await hydrateMermaid(fakeRoot([host]), () => calls.push('mutated'));
  check('hydrate: done host skipped, callback NOT fired', calls.length === 0, `calls=${calls.length}`);
  check('hydrate: done host content untouched', host.innerHTML === source, host.innerHTML);
}

{
  // React StrictMode replays a committed effect (setup -> cleanup -> setup)
  // against the SAME host. A cancelled pass must not leave a claim behind:
  // the mark means "settled", never "someone else is rendering", or the
  // replay would skip a connected host that was never rendered and the
  // diagram would stay source text forever.
  mermaidSvgCacheTest.reset();
  const calls: string[] = [];
  const host = fakeHost('graph TD\n  REPLAY --> SRC');
  const root = fakeRoot([host]);
  const first = new AbortController();
  const superseded = hydrateMermaid(root, () => calls.push('first'), first.signal);
  first.abort();
  await superseded;
  check('replay: a cancelled pass claims nothing', host.getAttribute('data-mermaid') === null, String(host.getAttribute('data-mermaid')));
  check('replay: a cancelled pass mutates nothing', host.innerHTML === 'graph TD\n  REPLAY --> SRC', host.innerHTML);
  check('replay: a cancelled pass fires no layout callback', calls.length === 0, `calls=${calls.length}`);
  await hydrateMermaid(root, () => calls.push('second'));
  check('replay: the replayed pass renders the connected host',
    host.innerHTML.includes('<svg') && host.classes.has('md-mermaid-host--rendered'), host.innerHTML);
  check('replay: the replayed pass settles the host', host.getAttribute('data-mermaid') === 'done');
  check('replay: the replayed pass fires the layout callback once', calls.length === 1, `calls=${JSON.stringify(calls)}`);
}

{
  // multiple hosts: one callback per mutation, in order
  const calls: string[] = [];
  const a = fakeHost('graph TD\n  A --> B');
  const b = fakeHost('BROKEN second');
  await hydrateMermaid(fakeRoot([a, b]), () => calls.push('mutated'));
  check('hydrate: per-host callback for each mutation', calls.length === 2, `calls=${calls.length}`);
  check('hydrate: first host rendered', a.innerHTML.includes('<svg'), a.innerHTML);
  check('hydrate: second host errored', b.innerHTML.includes('md-mermaid-error'), b.innerHTML);
}

{
  // Repeated invalid renders must not leak mermaid's parser-error DOM.
  // mermaid 11.16.1 appends a temporary wrapper (`#d<renderId>` holding the
  // `<svg id="<renderId>">` it draws into) to document.body and removes it
  // on success, but on a parse error it THROWS before the removal — the
  // parser-error SVG ("Syntax error in text", "mermaid version …") would
  // accumulate below the composer, one wrapper per failed render. The stub
  // reproduces that lifecycle against a fake document; hydrateMermaid must
  // remove exactly each render id's wrapper on both paths while the in-host
  // `.md-mermaid-host--error` fallback survives.
  const prevDoc = Reflect.get(globalThis, 'document');
  Object.defineProperty(globalThis, 'document', {
    value: fakeDocument,
    configurable: true,
    writable: true,
  });
  try {
    // 1) prove the stub reproduces the leak: a failed render leaves its
    //    wrapper (with the parser/version text) in the registry.
    let threw = false;
    try {
      await mermaidMock.render('mmd-probe', 'BROKEN probe');
    } catch {
      threw = true;
    }
    const probe = tempWrappers.find((n) => n.id === 'dmmd-probe');
    check('hydrate-leak: stub reproduces the wrapper leak on failure',
      threw && probe !== null && probe.textContent.includes('Syntax error in text'),
      `threw=${threw} probe=${probe?.id ?? 'missing'}`);
    probe?.remove();
    check('hydrate-leak: probe wrapper removed', !tempWrappers.some((n) => n.id === 'dmmd-probe'));

    // 2) repeated invalid renders through hydrateMermaid: every leaked
    //    wrapper is cleaned, the in-host error fallback remains, and the
    //    post-mutation callback still fires once per failed host.
    const calls: string[] = [];
    const a = fakeHost('BROKEN first');
    const b = fakeHost('BROKEN second');
    await hydrateMermaid(fakeRoot([a, b]), () => calls.push('mutated'));
    check('hydrate-leak: no wrapper left after repeated failures',
      tempWrappers.length === 0,
      `leftovers: ${tempWrappers.map((n) => n.id).join(',') || 'none'}`);
    check('hydrate-leak: no parser/version text remains in the fake body',
      !tempWrappers.some(
        (n) => n.textContent.includes('Syntax error in text') || n.textContent.includes('mermaid version')
      ));
    check('hydrate-leak: first host keeps its error fallback',
      a.innerHTML.includes('md-mermaid-error') && a.classes.has('md-mermaid-host--error'), a.innerHTML);
    check('hydrate-leak: second host keeps its error fallback',
      b.innerHTML.includes('md-mermaid-error') && b.classes.has('md-mermaid-host--error'), b.innerHTML);
    check('hydrate-leak: callback fired once per failed mutation',
      calls.length === 2, `calls=${calls.length}`);
  } finally {
    if (prevDoc === undefined) {
      Reflect.deleteProperty(globalThis, 'document');
    } else {
      Object.defineProperty(globalThis, 'document', {
        value: prevDoc,
        configurable: true,
        writable: true,
      });
    }
  }
}

// ---- mermaid bounds: input/source + bounded rendered-host policy ----
// The Web host is two-phase (renderFence placeholder -> hydrateMermaid SVG).
// Both phases mirror the Rust MAX_MERMAID_SOURCE_BYTES (64 KiB) source bound
// without duplicating the graph parser. Success is SVG-only; failures use the
// bounded `Mermaid · source fallback` block with `Could not render: <detail>`.
// strict security (escapeHtml redacts + escapes) and per-render-id wrapper
// cleanup remain required.

{
  // Input/source policy: renderFence rejects an oversize mermaid fence at the
  // input phase — emitting the bounded `Mermaid · source fallback` host
  // (claimed done, error class) instead of the async hydration host, so
  // hydrateMermaid never reaches mermaid.render and the streaming surface
  // never builds a giant host DOM. A normal-size fence still emits the plain
  // hydration host with no fallback label.
  const oversized = 'flowchart TD\n' + 'A --> B\n'.repeat(10_000);
  check('input-oversize: fixture exceeds 64 KiB',
    oversized.length > 64 * 1024, `len=${oversized.length}`);
  const html = renderFence('mermaid', oversized);
  check('input-oversize: fallback host emitted (host + error + done)',
    html.includes('md-mermaid-host')
      && html.includes('md-mermaid-host--error')
      && html.includes('data-mermaid="done"'),
    html);
  check('input-oversize: source-fallback note present',
    html.includes('Mermaid · source fallback'), html);
  check('input-oversize: Could not render reason present',
    html.includes('Could not render:'), html);
  check('input-oversize: reason states the 64 KiB limit',
    html.includes('65536 bytes'), html);
  check('input-oversize: fallback source bounded (truncation marker)',
    html.includes('(source truncated)'), html);
  check('input-oversize: host output stays bounded under the source size',
    html.length < oversized.length, `html=${html.length} src=${oversized.length}`);
  check('input-oversize: strict security (no script / event handler)',
    !/<script\b/i.test(html) && !/<[^>]+\son[a-z]+\s*=/i.test(html), html);
  // the oversize input host is claimed done, so hydrate skips it (no render)
  const spyCalls: { render: number } = { render: 0 };
  const origRender = mermaidMock.render;
  mermaidMock.render = (id: string, src: string) => {
    spyCalls.render += 1;
    return origRender(id, src);
  };
  try {
    const host = fakeHost(html); // a host built from the fallback output
    // simulate the DOM: renderFence output parsed into a host whose only
    // attribute is data-mermaid="done" (the input fallback's claim)
    host.setAttribute('data-mermaid', 'done');
    await hydrateMermaid(fakeRoot([host]));
    check('input-oversize: hydrate skips the done fallback (no render)',
      spyCalls.render === 0, `renderCalls=${spyCalls.render}`);
  } finally {
    mermaidMock.render = origRender;
  }

  // normal-size fence still emits the plain hydration host (no fallback)
  const normal = renderFence('mermaid', 'flowchart TD\nA --> B');
  check('input-normal: plain hydration host emitted',
    normal.includes('md-mermaid-host')
      && !normal.includes('md-mermaid-host--error')
      && !normal.includes('data-mermaid')
      && !normal.includes('Mermaid ·'),
    normal);
}

{
  // Rendered-host policy (defense-in-depth): a host carrying oversize source
  // directly (e.g. a restored transcript host) must NEVER reach mermaid.render
  // — hydrateMermaid rejects it before the render call and takes the bounded
  // escaped/redacted fallback. This proves the render callback is not invoked
  // for a >64 KiB source even when the input phase did not pre-reject it.
  const oversized = 'flowchart TD\n' + 'A --> B\n'.repeat(10_000);
  check('hydrate-oversize: fixture exceeds 64 KiB',
    oversized.length > 64 * 1024, `len=${oversized.length}`);
  const origRender = mermaidMock.render;
  let renderCalls = 0;
  mermaidMock.render = (id: string, src: string) => {
    renderCalls += 1;
    return origRender(id, src);
  };
  try {
    const calls: string[] = [];
    const host = fakeHost(oversized);
    await hydrateMermaid(fakeRoot([host]), () => calls.push('mutated'));
    check('hydrate-oversize: mermaid.render NOT invoked',
      renderCalls === 0, `renderCalls=${renderCalls}`);
    check('hydrate-oversize: callback fired once for the fallback mutation',
      calls.length === 1, `calls=${calls.length}`);
    check('hydrate-oversize: error class added',
      host.classes.has('md-mermaid-host--error'), host.innerHTML);
    check('hydrate-oversize: host claimed done',
      host.getAttribute('data-mermaid') === 'done');
    check('hydrate-oversize: source-fallback note present',
      host.innerHTML.includes('Mermaid · source fallback'), host.innerHTML);
    check('hydrate-oversize: Could not render reason present',
      host.innerHTML.includes('Could not render:'), host.innerHTML);
    check('hydrate-oversize: reason states the 64 KiB limit',
      host.innerHTML.includes('65536 bytes'), host.innerHTML);
    check('hydrate-oversize: fallback source bounded (truncation marker)',
      host.innerHTML.includes('(source truncated)'), host.innerHTML);
    check('hydrate-oversize: host output bounded under the source size',
      host.innerHTML.length < oversized.length,
      `html=${host.innerHTML.length} src=${oversized.length}`);
    check('hydrate-oversize: strict security (no script / event handler)',
      !/<script\b/i.test(host.innerHTML)
        && !/<[^>]+\son[a-z]+\s*=/i.test(host.innerHTML),
      host.innerHTML);
  } finally {
    mermaidMock.render = origRender;
  }
}

{
  // Successful hydration replaces the host with SVG only. Source fallback
  // chrome and Mermaid kind labels are failure-only presentation.
  const host = fakeHost('sequenceDiagram\nAlice ->> Bob: hi');
  await hydrateMermaid(fakeRoot([host]));
  check('success-svg: sequenceDiagram contains SVG only',
    host.innerHTML.includes('md-mermaid-viewport') && host.innerHTML.includes('<svg')
      && !host.innerHTML.includes('Mermaid ·')
      && !host.innerHTML.includes('source fallback'), host.innerHTML);
  check('success-svg: rendered class added',
    host.classes.has('md-mermaid-host--rendered'));
  check('success-svg: no error class',
    !host.classes.has('md-mermaid-host--error'));

  const flowchart = fakeHost('graph TD\nA --> B');
  await hydrateMermaid(fakeRoot([flowchart]));
  check('success-svg: graph alias also contains SVG only',
    flowchart.innerHTML.includes('md-mermaid-viewport') && flowchart.innerHTML.includes('<svg')
      && !flowchart.innerHTML.includes('Mermaid ·')
      && !flowchart.innerHTML.includes('source fallback'), flowchart.innerHTML);
}

{
  // Bounded `Could not render` reason: a render error whose message is long
  // is capped to a single bounded line in the fallback so a chatty mermaid
  // error never inflates the host. The reason still starts with
  // `Could not render:`, the bounded source fallback + note are present,
  // and the full long message is NOT carried into the host.
  const longMessage = 'x'.repeat(2_000);
  const origRender = mermaidMock.render;
  mermaidMock.render = () => { throw new Error(longMessage); };
  try {
    const host = fakeHost('flowchart TD\nA --> B');
    await hydrateMermaid(fakeRoot([host]));
    check('reason-bound: Could not render prefix present',
      host.innerHTML.includes('Could not render:'), host.innerHTML);
    check('reason-bound: note present',
      host.innerHTML.includes('Mermaid · source fallback'), host.innerHTML);
    check('reason-bound: full long reason NOT present (bounded)',
      !host.innerHTML.includes(longMessage), host.innerHTML);
    check('reason-bound: truncation marker present for the reason',
      host.innerHTML.includes('(source truncated)'), host.innerHTML);
    check('reason-bound: host output bounded under the reason size',
      host.innerHTML.length < longMessage.length,
      `html=${host.innerHTML.length}`);
    check('reason-bound: error class added',
      host.classes.has('md-mermaid-host--error'));
  } finally {
    mermaidMock.render = origRender;
  }
}

{
  // Redaction on the fallback path: credential-shaped source in a failed
  // (or oversize) diagram is redacted before display — escapeHtml() runs
  // redactSecrets() first, so the fallback never carries raw secrets. A
  // render that fails on a source containing an AWS-shaped key still shows
  // [REDACTED] (matching safeText), never the raw credential.
  const leakySource = 'flowchart TD\nA[AKIAIOSFODNN7EXAMPLE] --> B';
  const origRender = mermaidMock.render;
  mermaidMock.render = () => { throw new Error('parse failed'); };
  try {
    const host = fakeHost(leakySource);
    await hydrateMermaid(fakeRoot([host]));
    check('fallback-redact: raw AWS key NOT in host',
      !host.innerHTML.includes('AKIAIOSFODNN7EXAMPLE'), host.innerHTML);
    check('fallback-redact: redaction marker present (matches safeText)',
      safeText(leakySource).includes('[REDACTED]')
        && host.innerHTML.includes('[REDACTED]'),
      host.innerHTML);
  } finally {
    mermaidMock.render = origRender;
  }
}

// ---- mermaid SVG result cache: session-switch rehydration ----
// Successful sanitized SVG *templates* are cached (never DOM nodes / never the
// mermaid.render promise). Each host instantiates a unique ID namespace.
// Key = exact source + mermaid version/config/security identity. Cache hits
// apply SVG/classes/data and fire the layout callback synchronously.
// Concurrent identical sources share one render promise; a rejected promise
// is dropped (errors are not sticky). LRU is bounded by entry count AND
// UTF-16 byte budget of key+SVG. mermaidSvgCacheTest.reset() is the
// test-only seam (caps/identity/clear/epoch); production does not import it.


{
  mermaidSvgCacheTest.reset();
  const origRender = mermaidMock.render;
  let renderCalls = 0;
  mermaidMock.render = (id: string, src: string) => {
    renderCalls += 1;
    return origRender(id, src);
  };
  const source = 'graph TD\n  CACHE_REMOUNT --> X';
  try {
    for (let i = 0; i < 9; i++) {
      const host = fakeHost(source);
      const calls: string[] = [];
      const pending = hydrateMermaid(fakeRoot([host]), () => calls.push('mutated'));
      if (i === 0) {
        await pending;
        check('cache-remount: first hydrate invoked mermaid.render once',
          renderCalls === 1, `renderCalls=${renderCalls}`);
      } else {
        check(`cache-remount: hit ${i} fired layout callback synchronously`,
          calls.length === 1, `calls=${calls.length}`);
        check(`cache-remount: hit ${i} applied SVG`,
          host.innerHTML.includes('<svg'), host.innerHTML);
        check(`cache-remount: hit ${i} added rendered class`,
          host.classes.has('md-mermaid-host--rendered'));
        check(`cache-remount: hit ${i} claimed data-mermaid=done`,
          host.getAttribute('data-mermaid') === 'done');
        check(`cache-remount: hit ${i} has no error class`,
          !host.classes.has('md-mermaid-host--error'));
        await pending;
      }
    }
    check('cache-remount: 9 identical remounts called mermaid.render once',
      renderCalls === 1, `renderCalls=${renderCalls}`);
  } finally {
    mermaidMock.render = origRender;
    mermaidSvgCacheTest.reset();
  }
}

{
  mermaidSvgCacheTest.reset();
  const origRender = mermaidMock.render;
  let renderCalls = 0;
  mermaidMock.render = (id: string, src: string) => {
    renderCalls += 1;
    return origRender(id, src);
  };
  try {
    await hydrateMermaid(fakeRoot([fakeHost('graph TD\n  CACHE_SRC_A --> X')]));
    await hydrateMermaid(fakeRoot([fakeHost('graph TD\n  CACHE_SRC_B --> X')]));
    check('cache-miss: different source is a miss',
      renderCalls === 2, `renderCalls=${renderCalls}`);

    const identitySource = 'graph TD\n  CACHE_IDENTITY --> X';
    await hydrateMermaid(fakeRoot([fakeHost(identitySource)]));
    const afterSource = renderCalls;
    mermaidSvgCacheTest.reset({ identity: '11.0.0\0strict\0dark\0{}', clear: false });
    const idHost = fakeHost(identitySource);
    const idCalls: string[] = [];
    await hydrateMermaid(fakeRoot([idHost]), () => idCalls.push('mutated'));
    check('cache-miss: different version/config/security identity is a miss',
      renderCalls === afterSource + 1, `renderCalls=${renderCalls} afterSource=${afterSource}`);
    check('cache-miss: identity-miss still rendered SVG',
      idHost.innerHTML.includes('<svg') && idCalls.length === 1, idHost.innerHTML);
  } finally {
    mermaidMock.render = origRender;
    mermaidSvgCacheTest.reset();
  }
}

{
  mermaidSvgCacheTest.reset();
  const origRender = mermaidMock.render;
  let renderCalls = 0;
  let releaseGate: (() => void) | undefined;
  const gate = new Promise<void>((resolve) => { releaseGate = resolve; });
  mermaidMock.render = async (id: string, src: string) => {
    renderCalls += 1;
    await gate;
    return origRender(id, src);
  };
  try {
    const source = 'graph TD\n  CACHE_CONCURRENT --> X';
    const a = fakeHost(source);
    const b = fakeHost(source);
    const pending = Promise.all([
      hydrateMermaid(fakeRoot([a])),
      hydrateMermaid(fakeRoot([b])),
    ]);
    check('cache-concurrent: one shared mermaid.render promise',
      renderCalls === 1, `renderCalls=${renderCalls}`);
    releaseGate?.();
    await pending;
    check('cache-concurrent: both hosts received SVG',
      a.innerHTML.includes('<svg') && b.innerHTML.includes('<svg'),
      `a=${a.innerHTML} b=${b.innerHTML}`);
    check('cache-concurrent: still a single mermaid.render after both settled',
      renderCalls === 1, `renderCalls=${renderCalls}`);
  } finally {
    mermaidMock.render = origRender;
    mermaidSvgCacheTest.reset();
  }
}

{
  // DOM-based instantiate over the SANITIZED template. The cache boundary
  // (sanitizeMermaidSvg) has already stripped foreignObject/script, every
  // on* event handler, and every non-local URL; instantiate then parses the
  // sanitized template and rewrites only real attributes — IDREF lists,
  // fragment hrefs, local url(#knownId) tokens, and CSS id selectors.
  // Text, comments, and quoted CSS stay literal; prefixes never wrap.
  mermaidSvgCacheTest.reset();
  const origRender = mermaidMock.render;
  const hazardFixture = [
    '<svg id="mmd-0" xmlns="http://www.w3.org/2000/svg"',
    ' aria-labelledby="chart-title-mmd-0 extra"',
    ' aria-describedby="chart-desc-mmd-0"',
    ' aria-owns="chart-title-mmd-0"',
    ' aria-controls="mmd-0-clip"',
    ' aria-flowto="mmd-0-filter"',
    ' aria-activedescendant="chart-title-mmd-0"',
    ' aria-details="chart-desc-mmd-0"',
    ' aria-errormessage="chart-desc-mmd-0">',
    '<title id="chart-title-mmd-0">Title</title>',
    '<desc id="chart-desc-mmd-0">Desc</desc>',
    '<defs>',
    '<marker id="mmd-0-arrow"/>',
    '<clipPath id="mmd-0-clip"/>',
    '<filter id="mmd-0-filter"/>',
    '<mask id="mmd-0-mask"/>',
    '</defs>',
    '<style>#mmd-0 .edge{stroke:red}.paint{filter:url("#mmd-0-filter");marker-end:url(#mmd-0-arrow)} .x{content:"#mmd-0"} @import url("https://evil.example/import.css");</style>',
    '<foreignObject id="mmd-0-label"><div xmlns="http://www.w3.org/1999/xhtml">foreign label</div></foreignObject>',
    '<path onload="alert(1)" marker-end="url(#mmd-0-arrow)" clip-path="url(#mmd-0-clip)"',
    ' filter="url(#mmd-0-filter)" mask="url(#mmd-0-mask)"',
    ' fill="url(#mmd-0-clip)" stroke="url(#mmd-0-arrow)"',
    ' href="#mmd-0-clip" xlink:href="#mmd-0-arrow"/>',
    '<use href="https://example.com/icon.svg#external"/>',
    '<script>alert(2)</script>',
    '<text>keep url(#mmd-0-arrow) as text</text>',
    '<!-- keep url(#mmd-0-arrow) in comment -->',
    '</svg>',
  ].join('');
  mermaidMock.render = async () => ({ svg: hazardFixture });
  const collectIds = (html: string): string[] => (
    [...html.matchAll(/\sid="([^"]+)"/g)].map((m) => m[1])
  );
  const refsResolve = (html: string): boolean => {
    const ids = new Set(collectIds(html));
    const hrefs = [...html.matchAll(/\s(?:xlink:)?href="#([^"]+)"/g)].map((m) => m[1]);
    const attrUrls = [...html.matchAll(/\s(?:marker-end|clip-path|filter|mask|fill|stroke)="url\(#([^)]+)\)"/g)]
      .map((m) => m[1]);
    const aria = [...html.matchAll(/\saria-(?:labelledby|describedby|owns|controls|flowto|activedescendant|details|errormessage)="([^"]+)"/g)]
      .flatMap((m) => m[1].split(/\s+/).filter((t) => t && t !== 'extra'));
    return [...hrefs, ...attrUrls, ...aria].every((id) => ids.has(id));
  };
  try {
    const source = 'graph TD\n  CACHE_UNIQUE --> X';
    const a = fakeHost(source);
    const b = fakeHost(source);
    await Promise.all([
      hydrateMermaid(fakeRoot([a])),
      hydrateMermaid(fakeRoot([b])),
    ]);
    const idsA = collectIds(a.innerHTML);
    const idsB = collectIds(b.innerHTML);
    check('cache-unique: both hosts instantiated an SVG',
      a.innerHTML.includes('<svg') && b.innerHTML.includes('<svg'));
    check('cache-unique: simultaneous hosts have disjoint IDs',
      idsA.length >= 6 && idsB.length >= 6 && idsA.every((id) => !idsB.includes(id)),
      `a=${idsA.join(',')} b=${idsB.join(',')}`);
    check('cache-unique: A local refs resolve within A', refsResolve(a.innerHTML), a.innerHTML);
    check('cache-unique: B local refs resolve within B', refsResolve(b.innerHTML), b.innerHTML);
    check('cache-unique: foreignObject and its label are stripped',
      !/foreignobject/i.test(a.innerHTML) && !a.innerHTML.includes('foreign label'), a.innerHTML);
    check('cache-unique: script is stripped', !/<script\b/i.test(a.innerHTML), a.innerHTML);
    check('cache-unique: on* event handlers are stripped',
      !/<[^>]+\son[a-z]+\s*=/i.test(a.innerHTML), a.innerHTML);
    check('cache-unique: external URL attributes are stripped',
      !a.innerHTML.includes('example.com'), a.innerHTML);
    check('cache-unique: CSS @import is stripped', !a.innerHTML.includes('@import'), a.innerHTML);
    check('cache-unique: full ARIA IDREF attrs rewritten',
      /aria-labelledby="[^"]*chart-title/.test(a.innerHTML)
        && /aria-describedby="[^"]*chart-desc/.test(a.innerHTML)
        && /aria-owns="[^"]*chart-title/.test(a.innerHTML)
        && /aria-controls="[^"]*clip/.test(a.innerHTML)
        && /aria-flowto="[^"]*filter/.test(a.innerHTML)
        && /aria-activedescendant="[^"]*chart-title/.test(a.innerHTML)
        && /aria-details="[^"]*chart-desc/.test(a.innerHTML)
        && /aria-errormessage="[^"]*chart-desc/.test(a.innerHTML),
      a.innerHTML);
    check('cache-unique: visible url(#id) text is unchanged',
      a.innerHTML.includes('keep url(#mmd-0-arrow) as text')
        && b.innerHTML.includes('keep url(#mmd-0-arrow) as text'),
      a.innerHTML);
    check('cache-unique: comment url(#id) is unchanged',
      a.innerHTML.includes('<!-- keep url(#mmd-0-arrow) in comment -->'), a.innerHTML);
    check('cache-unique: quoted CSS #id is unchanged',
      a.innerHTML.includes('content:"#mmd-0"'), a.innerHTML);
    check('cache-unique: style selector ID is rewritten in serialized SVG',
      /<style>#mmd-i\d+-mmd-0 \.edge/.test(a.innerHTML), a.innerHTML);
    check('cache-unique: quoted and unquoted CSS url refs are rewritten',
      /filter:url\("#mmd-i\d+-mmd-0-filter"\)/.test(a.innerHTML)
        && /marker-end:url\(#mmd-i\d+-mmd-0-arrow\)/.test(a.innerHTML),
      a.innerHTML);
    check('cache-unique: aria extra token is preserved',
      /aria-labelledby="[^"]*\bextra\b/.test(a.innerHTML), a.innerHTML);

    const c = fakeHost(source);
    const cCalls: string[] = [];
    const pendingC = hydrateMermaid(fakeRoot([c]), () => cCalls.push('mutated'));
    check('cache-unique: cache-hit still instantiates a third namespace',
      cCalls.length === 1 && collectIds(c.innerHTML).every((id) => !idsA.includes(id) && !idsB.includes(id)),
      c.innerHTML);
    check('cache-unique: cache-hit refs resolve within C', refsResolve(c.innerHTML), c.innerHTML);
    await pendingC;

    mermaidSvgCacheTest.instanceSeq(1_000_000n);
    const d = fakeHost(source);
    const e = fakeHost(source);
    await hydrateMermaid(fakeRoot([d]));
    await hydrateMermaid(fakeRoot([e]));
    const idsD = collectIds(d.innerHTML);
    const idsE = collectIds(e.innerHTML);
    check('cache-unique: million+ prefix does not wrap',
      idsD.every((id) => id.startsWith('mmd-i1000001-'))
        && idsE.every((id) => id.startsWith('mmd-i1000002-')),
      `d=${idsD.join(',')} e=${idsE.join(',')}`);
    check('cache-unique: million+ prefixes stay unique',
      idsD.every((id) => !idsE.includes(id)),
      `d=${idsD.join(',')} e=${idsE.join(',')}`);
  } finally {
    mermaidMock.render = origRender;
    mermaidSvgCacheTest.reset();
    mermaidSvgCacheTest.instanceSeq(0n);
  }
}

{
  // Fail-closed sanitize: a parsererror document (the '&nbsp;' sentinel —
  // an undefined XML entity) and a missing DOMParser both reject the render
  // output at the cache boundary. The host degrades to the bounded, escaped
  // `Mermaid · source fallback` block with NO viewer toolbar and never the
  // raw (unsanitized) markup.
  mermaidSvgCacheTest.reset();
  const origRender = mermaidMock.render;
  const prevParser = Reflect.get(globalThis, 'DOMParser');
  try {
    mermaidMock.render = async () => ({ svg: '<svg id="raw-0"><foreignObject>raw</foreignObject>&nbsp;</svg>' });
    const parseError = fakeHost('graph TD\n  FAIL_PARSE --> X');
    await hydrateMermaid(fakeRoot([parseError]));
    check('fail-closed: parsererror degrades to the source fallback',
      parseError.innerHTML.includes('md-mermaid-error')
        && parseError.innerHTML.includes('Mermaid · source fallback')
        && parseError.innerHTML.includes('Could not render:'),
      parseError.innerHTML);
    check('fail-closed: parsererror fallback has no toolbar and no svg',
      !parseError.innerHTML.includes('md-mermaid-toolbar')
        && !parseError.innerHTML.includes('<svg'),
      parseError.innerHTML);
    check('fail-closed: parsererror fallback never carries raw markup',
      !parseError.innerHTML.includes('raw-0')
        && !/foreignobject/i.test(parseError.innerHTML),
      parseError.innerHTML);
    check('fail-closed: parsererror host keeps the error class',
      parseError.classes.has('md-mermaid-host--error')
        && parseError.getAttribute('data-mermaid') === 'done');

    Reflect.deleteProperty(globalThis, 'DOMParser');
    mermaidMock.render = async () => ({ svg: '<svg id="no-parser-0"></svg>' });
    const noParser = fakeHost('graph TD\n  FAIL_NO_PARSER --> X');
    await hydrateMermaid(fakeRoot([noParser]));
    check('fail-closed: missing DOMParser degrades to the source fallback',
      noParser.innerHTML.includes('md-mermaid-error')
        && noParser.innerHTML.includes('Mermaid · source fallback')
        && noParser.innerHTML.includes('Could not render:'),
      noParser.innerHTML);
    check('fail-closed: missing-DOMParser fallback has no toolbar and no svg',
      !noParser.innerHTML.includes('md-mermaid-toolbar')
        && !noParser.innerHTML.includes('<svg'),
      noParser.innerHTML);
    check('fail-closed: missing-DOMParser host keeps the error class',
      noParser.classes.has('md-mermaid-host--error'));
  } finally {
    if (prevParser === undefined) Reflect.deleteProperty(globalThis, 'DOMParser');
    else Object.defineProperty(globalThis, 'DOMParser', { value: prevParser, configurable: true, writable: true });
    mermaidMock.render = origRender;
    mermaidSvgCacheTest.reset();
  }
}



{
  mermaidSvgCacheTest.reset({ maxEntries: 2 });
  const origRender = mermaidMock.render;
  let renderCalls = 0;
  mermaidMock.render = (id: string, src: string) => {
    renderCalls += 1;
    return origRender(id, src);
  };
  try {
    const srcA = 'graph TD\n  CACHE_LRU_A --> X';
    const srcB = 'graph TD\n  CACHE_LRU_B --> X';
    const srcC = 'graph TD\n  CACHE_LRU_C --> X';
    await hydrateMermaid(fakeRoot([fakeHost(srcA)]));
    await hydrateMermaid(fakeRoot([fakeHost(srcB)]));
    await hydrateMermaid(fakeRoot([fakeHost(srcA)])); // touch A → B is LRU
    await hydrateMermaid(fakeRoot([fakeHost(srcC)])); // evicts B
    const afterFill = renderCalls;
    const hitA = fakeHost(srcA);
    const hitACalls: string[] = [];
    const pendingA = hydrateMermaid(fakeRoot([hitA]), () => hitACalls.push('mutated'));
    check('cache-lru: touched A survived eviction (sync hit)',
      hitACalls.length === 1 && hitA.innerHTML.includes('<svg') && renderCalls === afterFill,
      `calls=${hitACalls.length} renderCalls=${renderCalls}`);
    await pendingA;
    const missB = fakeHost(srcB);
    await hydrateMermaid(fakeRoot([missB]));
    check('cache-lru: untouched B was evicted (miss)',
      renderCalls === afterFill + 1, `renderCalls=${renderCalls} afterFill=${afterFill}`);
    check('cache-lru: B re-render still produces SVG',
      missB.innerHTML.includes('<svg'), missB.innerHTML);
  } finally {
    mermaidMock.render = origRender;
    mermaidSvgCacheTest.reset();
  }
}

{
  mermaidSvgCacheTest.reset();
  const origRender = mermaidMock.render;
  let renderCalls = 0;
  mermaidMock.render = (id: string, src: string) => {
    renderCalls += 1;
    return origRender(id, src);
  };
  try {
    const srcA = 'graph TD\n  CACHE_BYTE_A_' + 'A'.repeat(200) + ' --> X';
    const srcB = 'graph TD\n  CACHE_BYTE_B_' + 'B'.repeat(200) + ' --> X';
    await hydrateMermaid(fakeRoot([fakeHost(srcA)]));
    const budget = mermaidSvgCacheTest.bytes();
    check('cache-bytes: retained budget includes source/config key + SVG',
      budget > 0 && mermaidSvgCacheTest.size() === 1, `budget=${budget}`);
    mermaidSvgCacheTest.reset({ maxBytes: budget });
    renderCalls = 0;
    await hydrateMermaid(fakeRoot([fakeHost(srcA)]));
    check('cache-bytes: one key+SVG entry fills the exact budget',
      mermaidSvgCacheTest.bytes() === budget && mermaidSvgCacheTest.size() === 1,
      `bytes=${mermaidSvgCacheTest.bytes()} budget=${budget}`);
    await hydrateMermaid(fakeRoot([fakeHost(srcB)]));
    check('cache-bytes: inserting B evicted A under the key+SVG UTF-16 budget',
      mermaidSvgCacheTest.size() === 1 && renderCalls === 2,
      `size=${mermaidSvgCacheTest.size()} renderCalls=${renderCalls}`);
    const afterFill = renderCalls;
    await hydrateMermaid(fakeRoot([fakeHost(srcA)]));
    check('cache-bytes: evicted A is a miss',
      renderCalls === afterFill + 1, `renderCalls=${renderCalls} afterFill=${afterFill}`);
  } finally {
    mermaidMock.render = origRender;
    mermaidSvgCacheTest.reset();
  }
}

{
  mermaidSvgCacheTest.reset();
  const origRender = mermaidMock.render;
  let renderCalls = 0;
  mermaidMock.render = (id: string, src: string) => {
    renderCalls += 1;
    return origRender(id, src);
  };
  try {
    const source = 'BROKEN cache sticky';
    const first = fakeHost(source);
    await hydrateMermaid(fakeRoot([first]));
    check('cache-error: first failure rendered the fallback',
      first.innerHTML.includes('md-mermaid-error'), first.innerHTML);
    check('cache-error: first failure invoked mermaid.render',
      renderCalls === 1, `renderCalls=${renderCalls}`);
    const second = fakeHost(source);
    await hydrateMermaid(fakeRoot([second]));
    check('cache-error: failed promise is not sticky (second hydrate re-renders)',
      renderCalls === 2, `renderCalls=${renderCalls}`);
    check('cache-error: second failure still degrades (not a cached SVG hit)',
      second.innerHTML.includes('md-mermaid-error')
        && !second.classes.has('md-mermaid-host--rendered'),
      second.innerHTML);
  } finally {
    mermaidMock.render = origRender;
    mermaidSvgCacheTest.reset();
  }
}

{
  mermaidSvgCacheTest.reset();
  const origRender = mermaidMock.render;
  mermaidMock.render = () => { throw new Error('unhandled-probe'); };
  const unhandled: string[] = [];
  const onUnhandled = (reason: unknown) => {
    unhandled.push(reason instanceof Error ? reason.message : String(reason));
  };
  process.on('unhandledRejection', onUnhandled);
  try {
    const host = fakeHost('BROKEN unhandled');
    await hydrateMermaid(fakeRoot([host]));
    const { promise: drain, resolve: drained } = Promise.withResolvers<void>();
    setImmediate(drained);
    await drain;
    check('cache-unhandled: failed render does not emit unhandledRejection',
      unhandled.length === 0, `unhandled=${unhandled.join(',')}`);
    check('cache-unhandled: host still degrades to fallback',
      host.innerHTML.includes('md-mermaid-error'), host.innerHTML);
  } finally {
    process.off('unhandledRejection', onUnhandled);
    mermaidMock.render = origRender;
    mermaidSvgCacheTest.reset();
  }
}


{
  mermaidSvgCacheTest.reset();
  const prevDoc = Reflect.get(globalThis, 'document');
  Object.defineProperty(globalThis, 'document', {
    value: fakeDocument,
    configurable: true,
    writable: true,
  });
  const origRender = mermaidMock.render;
  let renderCalls = 0;
  mermaidMock.render = (id: string, src: string) => {
    renderCalls += 1;
    return origRender(id, src);
  };
  try {
    const source = 'graph TD\n  CACHE_LEAK --> X';
    for (let i = 0; i < 3; i++) {
      await hydrateMermaid(fakeRoot([fakeHost(source)]));
    }
    check('cache-leak: mermaid.render once for cached remounts',
      renderCalls === 1, `renderCalls=${renderCalls}`);
    check('cache-leak: success remounts leave no temp wrapper',
      tempWrappers.length === 0,
      `leftovers: ${tempWrappers.map((n) => n.id).join(',') || 'none'}`);

    const broken = 'BROKEN cache leak';
    const a = fakeHost(broken);
    const b = fakeHost(broken);
    await Promise.all([
      hydrateMermaid(fakeRoot([a])),
      hydrateMermaid(fakeRoot([b])),
    ]);
    check('cache-leak: concurrent failures share one render',
      renderCalls === 2, `renderCalls=${renderCalls}`);
    check('cache-leak: concurrent failures leave no temp wrapper',
      tempWrappers.length === 0,
      `leftovers: ${tempWrappers.map((n) => n.id).join(',') || 'none'}`);
    check('cache-leak: in-host error fallback survives wrapper cleanup',
      a.innerHTML.includes('md-mermaid-error') && b.innerHTML.includes('md-mermaid-error'),
      `a=${a.innerHTML} b=${b.innerHTML}`);

    mermaidSvgCacheTest.reset();
    const afterReset = fakeHost(source);
    await hydrateMermaid(fakeRoot([afterReset]));
    check('cache-reset: reset() dropped the prior SVG entry',
      renderCalls === 3, `renderCalls=${renderCalls}`);
  } finally {
    mermaidMock.render = origRender;
    mermaidSvgCacheTest.reset();
    if (prevDoc === undefined) {
      Reflect.deleteProperty(globalThis, 'document');
    } else {
      Object.defineProperty(globalThis, 'document', {
        value: prevDoc,
        configurable: true,
        writable: true,
      });
    }
  }
}

{
  mermaidSvgCacheTest.reset();
  const origRender = mermaidMock.render;
  let renderCalls = 0;
  let releaseGate: (() => void) | undefined;
  const gate = new Promise<void>((resolve) => { releaseGate = resolve; });
  mermaidMock.render = async (id: string, src: string) => {
    renderCalls += 1;
    await gate;
    return origRender(id, src);
  };
  try {
    const source = 'graph TD\n  CACHE_EPOCH --> X';
    const first = fakeHost(source);
    const pendingFirst = hydrateMermaid(fakeRoot([first]));
    mermaidSvgCacheTest.reset();
    releaseGate?.();
    await pendingFirst;
    check('cache-epoch: stale inflight still hydrates its host',
      first.innerHTML.includes('<svg'), first.innerHTML);
    check('cache-epoch: stale inflight did not put into the new cache',
      mermaidSvgCacheTest.size() === 0, `size=${mermaidSvgCacheTest.size()}`);
    const second = fakeHost(source);
    await hydrateMermaid(fakeRoot([second]));
    check('cache-epoch: next hydrate after reset is a miss',
      renderCalls === 2, `renderCalls=${renderCalls}`);
    check('cache-epoch: new render is admitted',
      mermaidSvgCacheTest.size() === 1, `size=${mermaidSvgCacheTest.size()}`);
  } finally {
    mermaidMock.render = origRender;
    mermaidSvgCacheTest.reset();
  }
}

{
  mermaidSvgCacheTest.reset();
  const origRender = mermaidMock.render;
  let renderCalls = 0;
  let releaseFirst: (() => void) | undefined;
  let releaseSecond: (() => void) | undefined;
  const firstGate = new Promise<void>((resolve) => { releaseFirst = resolve; });
  const secondGate = new Promise<void>((resolve) => { releaseSecond = resolve; });
  mermaidMock.render = async (id: string, src: string) => {
    renderCalls += 1;
    if (renderCalls === 1) await firstGate;
    else await secondGate;
    return origRender(id, src);
  };
  try {
    const source = 'graph TD\n  CACHE_EPOCH_ID --> X';
    const first = fakeHost(source);
    const pendingFirst = hydrateMermaid(fakeRoot([first]));
    mermaidSvgCacheTest.reset({ identity: '11.0.0\0strict\0dark\0{}', clear: false });
    const second = fakeHost(source);
    const pendingSecond = hydrateMermaid(fakeRoot([second]));
    check('cache-epoch: identity change starts a new inflight render',
      renderCalls === 2, `renderCalls=${renderCalls}`);
    releaseFirst?.();
    await pendingFirst;
    check('cache-epoch: old completion did not overwrite the new identity cache',
      mermaidSvgCacheTest.size() === 0, `size=${mermaidSvgCacheTest.size()}`);
    releaseSecond?.();
    await pendingSecond;
    check('cache-epoch: new identity render is admitted under the new key',
      mermaidSvgCacheTest.size() === 1, `size=${mermaidSvgCacheTest.size()}`);
    check('cache-epoch: old finally did not drop the new inflight',
      second.innerHTML.includes('<svg'), second.innerHTML);
  } finally {
    mermaidMock.render = origRender;
    mermaidSvgCacheTest.reset();
  }
}



// ---- hub wait card body: markdown renders structurally ----
// The Waiting/Hub wait tool card renders its body/note through THIS shared
// renderer. A typical settled wait body carries bullets with inline code and
// path lists — they must display as markdown (not raw plain text), hostile
// HTML must stay literal, and code fences must render.
{
  const body = [
    '- checked `crates/pi-cli/web/src/App.tsx`',
    '- verified the compact clamp',
    '',
    '```rust',
    'let cards = items.filter(|i| i.kind == "toolCard");',
    '```',
    '',
    'next: <script>alert(1)</script>, <img src=x onerror=alert(1)>, [x](javascript:alert(1))',
  ].join('\n');
  const html = renderMarkdown(body);
  check('hub body: bullet list renders', html.includes('<ul class="md-list">'), html.slice(0, 200));
  check('hub body: inline code renders', html.includes('data-md-path="crates/pi-cli/web/src/App.tsx"') && html.includes('>crates/pi-cli/web/src/App.tsx</code>'), html.slice(0, 400));
  check('hub body: code fence renders with hljs', html.includes('<pre class="md-fence__pre">') && hasHljsSpan(codeInner(html), 'let'), html.slice(0, 300));
  check('hub body: no raw <script>', !html.includes('<script'), html.slice(0, 200));
  check('hub body: no raw <img>', !html.includes('<img'), html.slice(0, 200));
  check('hub body: no javascript: href', !html.includes('javascript:'), html.slice(0, 200));
  check('hub body: hostile html stays literal', html.includes('&lt;script&gt;alert(1)&lt;/script&gt;') && html.includes('&lt;img src=x onerror=alert(1)&gt;'), html.slice(0, 300));
}

// ---- hub body bounded head + hint renders as clean markdown ----
// boundHubBody keeps the leading lines and appends the omitted-line hint;
// the bounded text must still render as well-formed markdown: the hint line
// closes an open list (renders as a paragraph) and never breaks rendering.
{
  const bounded = [
    '- one',
    '- two',
    '- three',
    '```text',
    'tail',
    '```',
    '\u2026 12 more lines',
  ].join('\n');
  const html = renderMarkdown(bounded);
  check('bounded hub body: list closed before the hint', html.includes('</ul>') && html.includes('\u2026 12 more lines'), html);
  check('bounded hub body: hint rendered as text, not list content', !/<li>\s*\u2026/.test(html), html.slice(0, 300));
  check('bounded hub body: no raw html anywhere', !html.includes('<script'), html);
}

// ---- hub body mermaid fence keeps the shared hydration host ----
{
  const html = renderMarkdown('waiting for:\n\n```mermaid\ngraph TD\n  A --> B\n```');
  check('hub body mermaid: host div emitted for hydrateMermaid', html.includes('md-mermaid-host') && html.includes('A --&gt; B'), html);
}

// ---- light-theme syntax colors meet WCAG AA on the actual fence fill ----
// GitHub Light hexes fail on `--bg-deep` / #ececec (keyword 3.87, built_in
// 2.95, comment 4.08, name/addition 3.92). The stylesheet must keep the
// github-dark token groups, darken only the failing families, and leave
// already-passing colors (and the whole dark theme) alone.
{
  const css = fs.readFileSync(path.join(process.cwd(), 'src/styles.css'), 'utf8');
  const colorByClass = new Map<string, string>();
  const ruleRe = /((?:\[data-theme="light"\]\s+[^{}]+))\{([^}]+)\}/g;
  for (const match of css.matchAll(ruleRe)) {
    const color = match[2]!.match(/(?:^|[^-])color:\s*(#[0-9a-fA-F]{6})\b/)?.[1]?.toLowerCase();
    if (!color) continue;
    for (const selector of match[1]!.split(',')) {
      const classes = [...selector.matchAll(/\.([\w.-]*hljs[\w.-]*)/g)];
      const cls = classes.at(-1)?.[1];
      if (cls) colorByClass.set(cls, color);
    }
  }

  const lin = (channel: number): number => {
    const c = channel / 255;
    return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  };
  const lum = (hex: string): number => {
    const n = hex.replace('#', '');
    return 0.2126 * lin(parseInt(n.slice(0, 2), 16))
      + 0.7152 * lin(parseInt(n.slice(2, 4), 16))
      + 0.0722 * lin(parseInt(n.slice(4, 6), 16));
  };
  const contrast = (fg: string, bg: string): number => {
    const a = lum(fg);
    const b = lum(bg);
    return (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
  };

  check('light hljs rules parsed', colorByClass.size >= 10, `${colorByClass.size} classes`);

  const FENCE = '#ececec';
  for (const [cls, color] of colorByClass) {
    const ratio = contrast(color, FENCE);
    check(
      `light ${cls} ${color} contrast >= 4.5:1 on ${FENCE}`,
      ratio + 1e-9 >= 4.5,
      `${ratio.toFixed(3)}:1`,
    );
  }

  check('light keyword family darkened to #a40e26', colorByClass.get('hljs-keyword') === '#a40e26');
  check('light built_in family darkened to #953800', colorByClass.get('hljs-built_in') === '#953800');
  check('light comment family darkened to #57606a', colorByClass.get('hljs-comment') === '#57606a');
  check('light name family darkened to #116329', colorByClass.get('hljs-name') === '#116329');
  check('light addition text matches name family', colorByClass.get('hljs-addition') === '#116329');
  check('light title family stays #6f42c1', colorByClass.get('hljs-title') === '#6f42c1');
  check('light attr/number family stays #005cc5', colorByClass.get('hljs-attr') === '#005cc5');
  check('light string family stays #032f62', colorByClass.get('hljs-string') === '#032f62');
  check('light default text stays #24292e', colorByClass.get('hljs') === '#24292e');
  check('light deletion stays #b31d28', colorByClass.get('hljs-deletion') === '#b31d28');
  check('light bullet stays #735c0f', colorByClass.get('hljs-bullet') === '#735c0f');

  const failingGithubLight = ['#d73a49', '#e36209', '#6a737d', '#22863a'];
  for (const hex of failingGithubLight) {
    check(
      `light hljs no longer uses failing GitHub Light ${hex}`,
      ![...colorByClass.values()].includes(hex),
    );
  }

  const okQuiet = css.match(/\[data-theme="light"\][\s\S]*?--ok-quiet:\s*(#[0-9a-fA-F]{6})\b/)?.[1]?.toLowerCase();
  check('light completed-state frame color parsed', okQuiet != null, okQuiet ?? 'missing');
  check('light --ok-quiet darkened to #6d906d', okQuiet === '#6d906d');
  check('light --ok-quiet no longer uses failing #8fae8f', okQuiet !== '#8fae8f');
  for (const surface of ['#ececec', '#f8f8f8', '#ffffff']) {
    const ratio = okQuiet ? contrast(okQuiet, surface) : 0;
    check(
      `light --ok-quiet ${okQuiet} contrast >= 3:1 on ${surface}`,
      ratio + 1e-9 >= 3,
      `${ratio.toFixed(3)}:1`,
    );
  }

  const families = [
    colorByClass.get('hljs-keyword'),
    colorByClass.get('hljs-built_in'),
    colorByClass.get('hljs-comment'),
    colorByClass.get('hljs-name'),
    colorByClass.get('hljs-title'),
    colorByClass.get('hljs-attr'),
    colorByClass.get('hljs-string'),
  ];
  check(
    'light token families stay distinct',
    new Set(families).size === families.length,
    families.join(','),
  );

  check(
    'dark theme hljs sheet is not restated as unscoped github-light',
    !/(?:^|\n)\.hljs-keyword\s*\{[^}]*#a40e26/.test(css),
  );
}

// ---- workspace path candidates + delegated click interceptor ----
{
  const parsed = parseWorkspacePathCandidate('src/app.ts');
  check('path: relative file', parsed?.path === 'src/app.ts' && parsed.line === undefined, JSON.stringify(parsed));

  const withLine = parseWorkspacePathCandidate('crates/pi-cli/web/src/markdown.ts:42');
  check('path:line', withLine?.path === 'crates/pi-cli/web/src/markdown.ts' && withLine.line === 42, JSON.stringify(withLine));

  const withCol = parseWorkspacePathCandidate('src/lib.rs:10:4');
  check(
    'path:line:col',
    withCol?.path === 'src/lib.rs' && withCol.line === 10 && withCol.column === 4,
    JSON.stringify(withCol),
  );

  check('reject javascript:', parseWorkspacePathCandidate('javascript:alert(1)') === null);
  check('reject data:', parseWorkspacePathCandidate('data:text/plain,hi') === null);
  check('reject file:', parseWorkspacePathCandidate('file:///etc/passwd') === null);
  check('reject ftp:', parseWorkspacePathCandidate('ftp://example.com/file.txt') === null);
  check('reject ssh:', parseWorkspacePathCandidate('ssh://host/src/app.ts') === null);
  check('reject custom-app:', parseWorkspacePathCandidate('custom-app:open') === null);
  check('reject javascript numeric payload', parseWorkspacePathCandidate('javascript:12') === null);
  check('href reject ftp:12', parseWorkspacePathCandidate('ftp:12', 'href') === null);
  check('href reject ssh:12', parseWorkspacePathCandidate('ssh:12', 'href') === null);
  check('href reject custom-app:12', parseWorkspacePathCandidate('custom-app:12', 'href') === null);
  check('href reject mixed-case scheme', parseWorkspacePathCandidate('FTP:12', 'href') === null);
  check('href reject plus scheme', parseWorkspacePathCandidate('svn+ssh:12', 'href') === null);
  check('href reject dot scheme', parseWorkspacePathCandidate('app.custom:12', 'href') === null);
  check('href reject ftp: punctuation-only', parseWorkspacePathCandidate('ftp:', 'href') === null);
  check('href reject data: punctuation-only', parseWorkspacePathCandidate('data:', 'href') === null);
  check('href reject custom-app: punctuation-only', parseWorkspacePathCandidate('custom-app:', 'href') === null);
  check('reject embedded whitespace', parseWorkspacePathCandidate('src/my file.ts:12') === null);
  check('reject control characters', parseWorkspacePathCandidate('src/app.ts\u0000:12') === null);
  check(
    'path:line file.ext',
    parseWorkspacePathCandidate('app.ts:42')?.path === 'app.ts'
      && parseWorkspacePathCandidate('app.ts:42')?.line === 42,
  );
  check(
    'path:line without extension',
    parseWorkspacePathCandidate('Makefile:12')?.path === 'Makefile'
      && parseWorkspacePathCandidate('Makefile:12')?.line === 12,
  );
  check(
    'path form with slash',
    parseWorkspacePathCandidate('crates/pi-cli/web/src/markdown.ts')?.path
      === 'crates/pi-cli/web/src/markdown.ts',
  );
  check('reject bare identifier', parseWorkspacePathCandidate('foo') === null);
  check('reject method call', parseWorkspacePathCandidate('foo()') === null);
  check('reject dotted method token', parseWorkspacePathCandidate('x.y') === null);
  check('reject type name', parseWorkspacePathCandidate('SomeType') === null);
  check(
    '1-char known language is a path',
    parseWorkspacePathCandidate('main.c')?.path === 'main.c'
      && parseWorkspacePathCandidate('main.c')?.line === undefined,
  );
  check('trailing slash on a file path is not a file', parseWorkspacePathCandidate('src/app.ts/') === null);
  check('href trailing slash is not a file', parseWorkspacePathCandidate('src/app.ts/', 'href') === null);
  check(
    'screenshot directory token is not a file path',
    parseWorkspacePathCandidate('guta_updates_backup/') === null,
  );
  check(
    'screenshot .bin file is a path',
    parseWorkspacePathCandidate('checkpoint_tree.bin')?.path === 'checkpoint_tree.bin'
      && parseWorkspacePathCandidate('checkpoint_tree.bin')?.line === undefined,
  );
  check(
    'screenshot line range opens at the start line',
    parseWorkspacePathCandidate('node_start_config.rs:37-41')?.path === 'node_start_config.rs'
      && parseWorkspacePathCandidate('node_start_config.rs:37-41')?.line === 37,
  );
  check(
    'cited basename range opens at the range start line',
    parseWorkspacePathCandidate('gatherer.rs:178-260')?.path === 'gatherer.rs'
      && parseWorkspacePathCandidate('gatherer.rs:178-260')?.line === 178,
    JSON.stringify(parseWorkspacePathCandidate('gatherer.rs:178-260')),
  );
  check(
    'dotted known extension is a path',
    parseWorkspacePathCandidate('app.ts')?.path === 'app.ts'
      && parseWorkspacePathCandidate('app.ts')?.line === undefined,
  );
  check(
    'code absolute with line is a candidate',
    parseWorkspacePathCandidate('/workspace/project/src/file.rs:299')?.path === '/workspace/project/src/file.rs'
      && parseWorkspacePathCandidate('/workspace/project/src/file.rs:299')?.line === 299,
  );
  check(
    'code Chinese path with line is a candidate',
    parseWorkspacePathCandidate('文档/文件.rs:299')?.path === '文档/文件.rs'
      && parseWorkspacePathCandidate('文档/文件.rs:299')?.line === 299,
  );
  check('href still rejects absolute', parseWorkspacePathCandidate('/workspace/rpi-feat-test/long.txt', 'href') === null);
  check('reject leading .. escape', parseWorkspacePathCandidate('../secret.ts') === null);
  check('reject interior .. escape', parseWorkspacePathCandidate('src/../secret.ts') === null);
  check('reject hash fragment', parseWorkspacePathCandidate('#section') === null);
  check('reject bare hash', parseWorkspacePathCandidate('#') === null);

  check('trim trailing .', parseWorkspacePathCandidate('src/app.ts.')?.path === 'src/app.ts');
  check('trim trailing comma', parseWorkspacePathCandidate('src/app.ts,')?.path === 'src/app.ts');
  check('trim trailing paren', parseWorkspacePathCandidate('src/app.ts)')?.path === 'src/app.ts');
  check('trim trailing colon', parseWorkspacePathCandidate('src/app.ts:')?.path === 'src/app.ts');
  check('trim trailing backtick', parseWorkspacePathCandidate('src/app.ts`')?.path === 'src/app.ts');
  check('trim then keep line', parseWorkspacePathCandidate('src/app.ts:12).')?.line === 12);

  const clampedZero = parseWorkspacePathCandidate('src/app.ts:0');
  check('clamp line 0 to 1', clampedZero?.path === 'src/app.ts' && clampedZero.line === 1, JSON.stringify(clampedZero));
  const clampedNeg = parseWorkspacePathCandidate('src/app.ts:0:0');
  check('clamp col 0 to 1', clampedNeg?.line === 1 && clampedNeg.column === 1, JSON.stringify(clampedNeg));

  check('workspace relative exact unchanged', markdownWorkspaceRelativePath('./src/lib.rs', undefined) === './src/lib.rs');
  check('workspace relative backslashes unchanged', markdownWorkspaceRelativePath(String.raw`src\lib.rs`, '/workspace/project') === String.raw`src\lib.rs`);
  check('workspace absolute descendant becomes relative', markdownWorkspaceRelativePath('/workspace/project/src/app.ts', '/workspace/project') === 'src/app.ts');
  check('workspace separators canonicalize', markdownWorkspaceRelativePath(String.raw`C:\workspace\project\src\app.ts`, 'C:/workspace/project/') === 'src/app.ts');
  check('workspace exact cwd rejected', markdownWorkspaceRelativePath('/workspace/project', '/workspace/project') === null);
  check('workspace outside sibling rejected at boundary', markdownWorkspaceRelativePath('/workspace/project-other/src/app.ts', '/workspace/project') === null);
  check('workspace absolute missing cwd rejected', markdownWorkspaceRelativePath('/workspace/project/src/app.ts', undefined) === null);
  check('workspace traversal rejected', markdownWorkspaceRelativePath('/workspace/project/src/../secret.ts', '/workspace/project') === null);

  // ---- bare-basename completion from the same rendered message ----
  // A cited `gatherer.rs:178-260` is a basename the server can only resolve
  // by unique-basename search; the grep hit beside it names the workspace
  // identity the click meant.
  const grepMessage = [
    'grep hit around `gatherer.rs:178-260`:',
    'src/queue/gatherer.rs-178-    pub async fn handle(',
    'src/queue/gatherer.rs-185-        let job = next().await;',
  ].join('\n');
  check(
    'nearby grep hit completes a bare basename',
    markdownNearbyPathCompletion('gatherer.rs', grepMessage) === 'src/queue/gatherer.rs',
    String(markdownNearbyPathCompletion('gatherer.rs', grepMessage)),
  );
  check(
    'colon-form citation completes a bare basename',
    markdownNearbyPathCompletion('gatherer.rs', 'see src/queue/gatherer.rs:178 for the handler')
      === 'src/queue/gatherer.rs',
  );
  check(
    'grep line content right after the line number still completes',
    markdownNearbyPathCompletion('gatherer.rs', 'src/queue/gatherer.rs-178-fn handle() {}')
      === 'src/queue/gatherer.rs',
  );
  check(
    'a path followed by prose completes the bare basename',
    markdownNearbyPathCompletion('gatherer.rs', 'see src/queue/gatherer.rs here') === 'src/queue/gatherer.rs',
  );
  check(
    'a trailing sentence period is trimmed and still completes',
    markdownNearbyPathCompletion('gatherer.rs', 'see src/queue/gatherer.rs.') === 'src/queue/gatherer.rs',
  );
  check(
    'the same path cited twice completes once',
    markdownNearbyPathCompletion('gatherer.rs', 'src/queue/gatherer.rs then ./src/queue/gatherer.rs')
      === 'src/queue/gatherer.rs',
  );
  check(
    'two distinct longer paths never complete',
    markdownNearbyPathCompletion('gatherer.rs', 'src/queue/gatherer.rs and legacy/gatherer.rs') === null,
  );
  check(
    'a longer segment is not a completion',
    markdownNearbyPathCompletion('gatherer.rs', 'see src/queue/gatherer.rs.bak notes') === null,
  );
  check(
    'a directory component is not a completion',
    markdownNearbyPathCompletion('gatherer.rs', 'see src/queue/gatherer.rs/child.rs notes') === null,
  );
  check(
    'an absolute-only context never completes',
    markdownNearbyPathCompletion('gatherer.rs', 'see /workspace/project/src/queue/gatherer.rs here') === null,
  );
  check(
    'a lone basename has nothing to complete with',
    markdownNearbyPathCompletion('gatherer.rs', 'the range gatherer.rs:178-260 only') === null,
  );
  check(
    'a path-shaped candidate is never completed',
    markdownNearbyPathCompletion('src/queue/gatherer.rs', grepMessage) === null,
  );
  check(
    'a homonym basename in the same message does not complete',
    markdownNearbyPathCompletion('gather.rs', grepMessage) === null,
  );
  check(
    'an absolute Unicode token never completes from its relative tail',
    markdownNearbyPathCompletion('gatherer.rs', '/home/用户/src/queue/gatherer.rs') === null,
  );
  check(
    'a backslash directory tail is not a completion',
    markdownNearbyPathCompletion('gatherer.rs', 'src/queue/gatherer.rs\\child.rs') === null,
  );
  check(
    'a truncated suffix of an overlong token is not a completion',
    markdownNearbyPathCompletion('gatherer.rs', `${'n'.repeat(600)}/src/queue/gatherer.rs`) === null,
  );
  check(
    'a Unicode relative token completes as one path',
    markdownNearbyPathCompletion('gatherer.rs', '见 用户/src/queue/gatherer.rs 处') === '用户/src/queue/gatherer.rs',
  );
  check(
    'a backslash separator on the leading side still completes',
    markdownNearbyPathCompletion('gatherer.rs', 'src\\queue\\gatherer.rs notes') === 'src\\queue\\gatherer.rs',
  );
  check(
    'an incomplete grep dash-digit tail is not a completion',
    markdownNearbyPathCompletion('gatherer.rs', 'src/queue/gatherer.rs-1foo notes') === null,
  );
  check(
    'a grep path:line:content token completes',
    markdownNearbyPathCompletion('gatherer.rs', 'src/queue/gatherer.rs:178:pub async fn handle(')
      === 'src/queue/gatherer.rs',
  );
  check(
    'a grep content slash is not a completion suffix',
    markdownNearbyPathCompletion('gatherer.rs', 'src/queue/gatherer.rs:178:see/other') === null,
  );
  check(
    'a drive-absolute token never completes from its relative tail',
    markdownNearbyPathCompletion('gatherer.rs', 'C:\\home\\src\\queue\\gatherer.rs') === null,
  );
  check(
    'a directory path without a longer unique suffix is not a completion',
    markdownCompletedWorkspacePath('src/queue/process_block.rs', '') === null,
  );
  check(
    'completed basename needs a unique nearby path',
    markdownCompletedWorkspacePath('process_block.rs', 'see src/runtime/process_block.rs:472') === 'src/runtime/process_block.rs',
  );
  check(
    'grep path:line:content still completes the basename',
    markdownCompletedWorkspacePath(
      'process_block.rs',
      'src/runtime/process_block.rs:472:fn handle(',
    ) === 'src/runtime/process_block.rs',
  );
  check(
    'ambiguous basename is not a completed path',
    markdownCompletedWorkspacePath(
      'process_block.rs',
      'src/runtime/process_block.rs and crates/other/process_block.rs',
    ) === null,
  );
  check(
    'lone basename is not a completed path',
    markdownCompletedWorkspacePath('process_block.rs', 'see `process_block.rs:472-480` only') === null,
  );
  const browserPhrase = 'Inspect `nested/target.rs:1`. The complete file path is `pkg/nested/target.rs`.';
  check(
    'shortened same-context suffix is a completion, not the open path',
    markdownCompletedWorkspacePath('nested/target.rs', browserPhrase) === 'pkg/nested/target.rs',
    String(markdownCompletedWorkspacePath('nested/target.rs', browserPhrase)),
  );
  check(
    'shortened suffix keeps the cited path and line, stamps the completion',
    renderMarkdown(browserPhrase).includes('data-md-path="nested/target.rs"')
      && renderMarkdown(browserPhrase).includes('data-md-completion="pkg/nested/target.rs"')
      && renderMarkdown(browserPhrase).includes('data-md-line="1"')
      && renderMarkdown(browserPhrase).includes('>nested/target.rs:1</code>'),
    renderMarkdown(browserPhrase),
  );
  const fl63 = 'duplicate `dup-name.txt:1` beside pkg/a/dup-name.txt and pkg/b/dup-name.txt\noutside `/tmp/work/outside-only.txt:1`';
  const fl63Html = renderMarkdown(fl63);
  check(
    'duplicate basename across a paragraph boundary stays unresolved',
    fl63Html.includes('data-md-resolved="none"')
      && !fl63Html.includes('data-md-path="pkg/a/dup-name.txt"')
      && !fl63Html.includes('data-md-path="pkg/b/dup-name.txt"'),
    fl63Html,
  );
  const fl63NoLine = 'duplicate `dup-name.txt` beside pkg/a/dup-name.txt and pkg/b/dup-name.txt\noutside `/tmp/work/outside-only.txt`';
  const fl63NoLineHtml = renderMarkdown(fl63NoLine);
  check(
    'no-line duplicate basename across paragraphs stays unresolved',
    fl63NoLineHtml.includes('data-md-resolved="none"')
      && fl63NoLineHtml.includes('>dup-name.txt</code>')
      && !fl63NoLineHtml.includes('data-md-path="pkg/a/dup-name.txt"')
      && !fl63NoLineHtml.includes('data-md-path="pkg/b/dup-name.txt"'),
    fl63NoLineHtml,
  );
  const fl63Unique = 'see `foo.rs` beside src/foo.rs\nnext paragraph';
  const fl63UniqueHtml = renderMarkdown(fl63Unique);
  check(
    'no-line unique basename is the resolved path without a line',
    fl63UniqueHtml.includes('data-md-path="src/foo.rs"')
      && fl63UniqueHtml.includes('>foo.rs</code>')
      && !fl63UniqueHtml.includes('data-md-line=')
      && !fl63UniqueHtml.includes('data-md-resolved='),
    fl63UniqueHtml,
  );
  const fl63Abs = 'see `/tmp/work/outside-only.txt`\nnext pkg/outside-only.txt';
  const fl63AbsHtml = renderMarkdown(fl63Abs);
  check(
    'no-line absolute code path gets no contextual completion',
    fl63AbsHtml.includes('data-md-path="/tmp/work/outside-only.txt"')
      && !fl63AbsHtml.includes('data-md-completion=')
      && !fl63AbsHtml.includes('data-md-line='),
    fl63AbsHtml,
  );
  check(
    'a longer lookalike is only a candidate; it does not prove the exact path is missing',
    markdownCompletedWorkspacePath(
      'pkg/nested/target.rs',
      'see pkg/nested/target.rs and vendor/pkg/nested/target.rs',
    ) === 'vendor/pkg/nested/target.rs',
  );
  check(
    'an outside absolute citation is not a completion',
    markdownCompletedWorkspacePath(
      '/workspace/pkg/nested/target.rs',
      'the file is pkg/nested/target.rs',
    ) === null,
  );
  check(
    'a dotdot citation is not a completion',
    markdownCompletedWorkspacePath('../pkg/nested/target.rs', 'pkg/nested/target.rs') === null,
  );
  check(
    'a partial segment is not a shortened suffix',
    markdownCompletedWorkspacePath('ed/target.rs', 'pkg/nested/target.rs') === null,
  );


  function fakeNode(opts: {
    text?: string | null;
    href?: string | null;
    dataHref?: string | null;
    dataMdPath?: string | null;
    dataMdLine?: string | null;
    dataMdResolved?: string | null;
    sessionId?: string;
    workspaceCwd?: string;
    /** textContent of the rendered owner root ([data-session-id]): the
     *  surrounding message text a bare-basename click is completed from. */
    rootText?: string | null;
    kind: 'code' | 'anchor' | 'download' | 'fence-code' | 'other';
  }) {
    const owner = {
      textContent: opts.rootText ?? null,
      getAttribute(name: string): string | null {
        if (name === 'data-session-id') return opts.sessionId ?? null;
        if (name === 'data-workspace-cwd') return opts.workspaceCwd ?? null;
        return null;
      },
      closest(): unknown { return null; },
    };
    const node = {
      textContent: opts.text ?? null,
      getAttribute(name: string): string | null {
        if (name === 'href') return opts.href ?? null;
        if (name === 'data-href') return opts.dataHref ?? null;
        if (name === 'data-md-path') return opts.dataMdPath ?? null;
        if (name === 'data-md-line') return opts.dataMdLine ?? null;
        if (name === 'data-md-resolved') return opts.dataMdResolved ?? null;
        return null;
      },
      closest(sel: string): unknown {
        if (sel === '[data-session-id]') return opts.sessionId ? owner : null;
        if (sel === '[data-md-path]') return opts.dataMdPath ? node : null;
        if (sel === '.md-link-download') return opts.kind === 'download' ? node : null;
        if (sel === 'code.md-code' || sel === '.md-code') {
          return opts.kind === 'code' || opts.kind === 'fence-code' ? node : null;
        }
        if (sel === '.md-fence') return opts.kind === 'fence-code' ? node : null;
        if (sel === 'a') return opts.kind === 'anchor' ? node : null;
      },
    };
    return node;
  }

  function click(target: unknown) {
    const event = {
      target,
      defaultPrevented: false,
      preventDefault() { event.defaultPrevented = true; },
    };
    return event;
  }

  const opened: Array<{ path: string; line?: number; sessionId: string; workspaceCwd?: string; completion?: string }> = [];
  const onOpen = (
    path: string,
    line: number | undefined,
    owner: { sessionId: string; workspaceCwd?: string },
    completion?: string,
  ) => {
    opened.push({ path, line, ...owner, completion });
  };

  opened.length = 0;
  const codeEvent = click(fakeNode({ kind: 'code', text: 'src/app.ts:8', sessionId: 'session-a' }));
  check('code span click handled', dispatchMarkdownPathClick(codeEvent, onOpen) === true);
  check('code span prevents default', codeEvent.defaultPrevented === true);
  check(
    'code span opens path:line for its rendered session',
    opened.length === 1 && opened[0].path === 'src/app.ts' && opened[0].line === 8
      && opened[0].sessionId === 'session-a',
    JSON.stringify(opened),
  );

  opened.length = 0;
  const relEvent = click(fakeNode({ kind: 'anchor', href: './src/lib.rs', sessionId: 'session-a' }));
  check('relative anchor handled', dispatchMarkdownPathClick(relEvent, onOpen) === true);
  check(
    'relative anchor opens ./ path',
    opened.length === 1 && opened[0].path === './src/lib.rs',
    JSON.stringify(opened),
  );

  opened.length = 0;
  const wsEvent = click(fakeNode({ kind: 'anchor', href: 'src/main.rs:3:1', sessionId: 'session-a' }));
  check('workspace-relative href handled', dispatchMarkdownPathClick(wsEvent, onOpen) === true);
  check('workspace-relative href line', opened[0]?.path === 'src/main.rs' && opened[0]?.line === 3, JSON.stringify(opened));

  opened.length = 0;
  const fileLineEvent = click(fakeNode({ kind: 'anchor', href: 'app.ts:8', sessionId: 'session-a' }));
  check(
    'file.ext:line href is RFC scheme and stays inert',
    dispatchMarkdownPathClick(fileLineEvent, onOpen) === false
      && opened.length === 0
      && fileLineEvent.defaultPrevented === false,
  );

  opened.length = 0;
  const pathFormEvent = click(fakeNode({ kind: 'code', text: 'crates/pi-cli/web/src/markdown.ts', dataMdPath: 'crates/pi-cli/web/src/markdown.ts', sessionId: 'session-a' }));
  check(
    'path form code span handled',
    dispatchMarkdownPathClick(pathFormEvent, onOpen) === true
      && opened[0]?.path === 'crates/pi-cli/web/src/markdown.ts'
      && opened[0]?.line === undefined,
    JSON.stringify(opened),
  );

  opened.length = 0;
  const makefileEvent = click(fakeNode({ kind: 'code', text: 'Makefile:12', dataMdResolved: 'none', sessionId: 'session-a' }));
  check(
    'Makefile:12 without a unique path is not a click',
    dispatchMarkdownPathClick(makefileEvent, onOpen) === false && opened.length === 0,
    JSON.stringify(opened),
  );

  opened.length = 0;
  const makefileCompleted = click(fakeNode({
    kind: 'code',
    text: 'Makefile:12',
    dataMdPath: 'docs/Makefile',
    dataMdLine: '12',
    sessionId: 'session-a',
    rootText: 'docs/Makefile notes and vendor/docs/Makefile',
  }));
  check(
    'Makefile:12 opens the unique nearby path directly',
    dispatchMarkdownPathClick(makefileCompleted, onOpen) === true
      && opened[0]?.path === 'docs/Makefile'
      && opened[0]?.completion === undefined
      && opened[0]?.line === 12,
    JSON.stringify(opened),
  );

  opened.length = 0;
  const appTsEvent = click(fakeNode({ kind: 'code', text: 'app.ts:42', dataMdResolved: 'none', sessionId: 'session-a' }));
  check(
    'app.ts:42 without a unique path is not a click',
    dispatchMarkdownPathClick(appTsEvent, onOpen) === false && opened.length === 0,
    JSON.stringify(opened),
  );
  opened.length = 0;
  const citedLineEvent = click(fakeNode({
    kind: 'code',
    text: 'file.rs:299',
    dataMdPath: 'src/file.rs',
    dataMdLine: '299',
    sessionId: 'session-a',
  }));
  check(
    'path:line data-md-path opens at that line',
    dispatchMarkdownPathClick(citedLineEvent, onOpen) === true
      && opened[0]?.path === 'src/file.rs'
      && opened[0]?.line === 299,
    JSON.stringify(opened),
  );
  opened.length = 0;
  const absCitedEvent = click(fakeNode({
    kind: 'code',
    text: '/workspace/project/src/file.rs:299',
    dataMdPath: '/workspace/project/src/file.rs',
    dataMdLine: '299',
    sessionId: 'session-a',
    workspaceCwd: '/workspace/project',
  }));
  check(
    'workspace-absolute path:line opens the relative file at that line',
    dispatchMarkdownPathClick(absCitedEvent, onOpen) === true
      && opened[0]?.path === 'src/file.rs'
      && opened[0]?.line === 299,
    JSON.stringify(opened),
  );
  opened.length = 0;
  const bareIdentEvent = click(fakeNode({ kind: 'code', text: 'foo', sessionId: 'session-a' }));
  check(
    'bare identifier code span stays inert',
    dispatchMarkdownPathClick(bareIdentEvent, onOpen) === false
      && opened.length === 0
      && bareIdentEvent.defaultPrevented === false,
  );
  opened.length = 0;
  const callEvent = click(fakeNode({ kind: 'code', text: 'foo()', sessionId: 'session-a' }));
  check(
    'method-call code span stays inert',
    dispatchMarkdownPathClick(callEvent, onOpen) === false
      && opened.length === 0
      && callEvent.defaultPrevented === false,
  );
  opened.length = 0;
  const dottedCallEvent = click(fakeNode({ kind: 'code', text: 'x.y', sessionId: 'session-a' }));
  check(
    'dotted method token stays inert',
    dispatchMarkdownPathClick(dottedCallEvent, onOpen) === false
      && opened.length === 0,
  );
  opened.length = 0;
  const dirEvent = click(fakeNode({ kind: 'code', text: 'guta_updates_backup/', sessionId: 'session-a' }));
  check(
    'trailing-slash directory code span stays inert',
    dispatchMarkdownPathClick(dirEvent, onOpen) === false
      && opened.length === 0
      && dirEvent.defaultPrevented === false,
  );
  opened.length = 0;
  const slashedFileEvent = click(fakeNode({ kind: 'code', text: 'src/app.ts/', sessionId: 'session-a' }));
  check(
    'trailing-slash file-shaped code span stays inert',
    dispatchMarkdownPathClick(slashedFileEvent, onOpen) === false
      && opened.length === 0
      && slashedFileEvent.defaultPrevented === false,
  );
  opened.length = 0;
  const rangeEvent = click(fakeNode({ kind: 'code', text: 'node_start_config.rs:37-41', dataMdResolved: 'none', sessionId: 'session-a' }));
  check(
    'line-range basename without a unique path is not a click',
    dispatchMarkdownPathClick(rangeEvent, onOpen) === false && opened.length === 0,
    JSON.stringify(opened),
  );
  opened.length = 0;
  const rangeCompleted = click(fakeNode({
    kind: 'code',
    text: 'node_start_config.rs:37-41',
    dataMdPath: 'crates/pi-cli/src/node_start_config.rs',
    dataMdLine: '37',
    sessionId: 'session-a',
    rootText: 'other/node_start_config.rs beside the stamp',
  }));
  check(
    'line-range basename opens the unique nearby path directly',
    dispatchMarkdownPathClick(rangeCompleted, onOpen) === true
      && opened.length === 1
      && opened[0].path === 'crates/pi-cli/src/node_start_config.rs'
      && opened[0].completion === undefined
      && opened[0].line === 37,
    JSON.stringify(opened),
  );
  opened.length = 0;
  const siblingStamped = click(fakeNode({
    kind: 'code',
    text: 'process_block.rs:472-480',
    dataMdPath: 'src/runtime/process_block.rs',
    dataMdLine: '472',
    sessionId: 'session-a',
    rootText: 'see `process_block.rs:472-480`',
  }));
  check(
    'stamped completed path opens from the assistant bubble alone',
    dispatchMarkdownPathClick(siblingStamped, onOpen) === true
      && opened.length === 1
      && opened[0].path === 'src/runtime/process_block.rs'
      && opened[0].line === 472,
    JSON.stringify(opened),
  );

  for (const [name, href] of [
    ['javascript', 'javascript:alert(1)'],
    ['data', 'data:text/plain,hi'],
    ['file', 'file:///etc/passwd'],
    ['ftp', 'ftp://example.com/file.txt'],
    ['ssh', 'ssh://host/src/app.ts'],
    ['custom-app', 'custom-app:open'],
    ['ftp-numeric', 'ftp:12'],
    ['ssh-numeric', 'ssh:12'],
    ['custom-app-numeric', 'custom-app:12'],
    ['mixed-case-scheme', 'FTP:12'],
    ['plus-scheme', 'svn+ssh:12'],
    ['dot-scheme', 'app.custom:12'],
    ['ftp-punct', 'ftp:'],
    ['data-punct', 'data:'],
    ['custom-app-punct', 'custom-app:'],
    ['absolute', '/etc/passwd'],
    ['dotdot', '../secret.ts'],
    ['hash', '#section'],
    ['bare-hash', '#'],
  ] as const) {
    opened.length = 0;
    const ev = click(fakeNode({ kind: 'anchor', href, sessionId: 'session-a' }));
    check(`reject ${name} anchor`, dispatchMarkdownPathClick(ev, onOpen) === false && opened.length === 0 && ev.defaultPrevented === false);
  }

  for (const [name, text] of [
    ['ftp', 'ftp://example.com/file.txt'],
    ['ssh', 'ssh://host/src/app.ts'],
    ['custom-app', 'custom-app:open'],
  ] as const) {
    opened.length = 0;
    const ev = click(fakeNode({ kind: 'code', text, sessionId: 'session-a' }));
    check(`reject ${name} code span`, dispatchMarkdownPathClick(ev, onOpen) === false && opened.length === 0 && ev.defaultPrevented === false);
  }
  opened.length = 0;
  const fenceEvent = click(fakeNode({ kind: 'fence-code', text: 'src/app.ts', sessionId: 'session-a' }));
  check('fence code stays inert', dispatchMarkdownPathClick(fenceEvent, onOpen) === false && opened.length === 0);

  opened.length = 0;
  const punctEvent = click(fakeNode({ kind: 'code', text: 'src/app.ts).', sessionId: 'session-a' }));
  check('code span trims punctuation', dispatchMarkdownPathClick(punctEvent, onOpen) === true && opened[0]?.path === 'src/app.ts', JSON.stringify(opened));

  opened.length = 0;
  const clampEvent = click(fakeNode({ kind: 'code', text: 'src/app.ts:0', sessionId: 'session-a' }));
  check('code span clamps line', dispatchMarkdownPathClick(clampEvent, onOpen) === true && opened[0]?.line === 1, JSON.stringify(opened));

  const subscribed: string[] = [];
  const unsub = subscribeMarkdownOpenPath((path) => { subscribed.push(path); });
  const subEvent = click(fakeNode({ kind: 'code', text: 'lib/mod.rs', sessionId: 'session-a' }));
  check('subscribeMarkdownOpenPath receives click', dispatchMarkdownPathClick(subEvent) === true && subscribed[0] === 'lib/mod.rs', JSON.stringify(subscribed));
  unsub();
  subscribed.length = 0;
  const afterUnsub = click(fakeNode({ kind: 'code', text: 'lib/mod.rs', sessionId: 'session-a' }));
  check('unsubscribe stops delivery', dispatchMarkdownPathClick(afterUnsub) === false && subscribed.length === 0);

  const refHits: Array<{ path: string; line?: number; sessionId: string; workspaceCwd?: string }> = [];
  const ownerHandler = (path: string, line: number | undefined, owner: { sessionId: string; workspaceCwd?: string }) => {
    refHits.push({ path, line, ...owner });
  };
  const unsubA = subscribeMarkdownOpenPath(ownerHandler);
  const unsubB = subscribeMarkdownOpenPath(ownerHandler);
  const ownerEvent = click(fakeNode({ kind: 'code', text: 'src/owned.ts:4', sessionId: 'session-owner' }));
  check(
    'ref-counted subscribe delivers once with owner session',
    dispatchMarkdownPathClick(ownerEvent) === true
      && refHits.length === 1
      && refHits[0].path === 'src/owned.ts'
      && refHits[0].line === 4
      && refHits[0].sessionId === 'session-owner',
    JSON.stringify(refHits),
  );
  unsubA();
  refHits.length = 0;
  const stillLive = click(fakeNode({ kind: 'code', text: 'src/owned.ts', sessionId: 'session-owner' }));
  check('ref-count keeps handler after first unsub', dispatchMarkdownPathClick(stillLive) === true && refHits.length === 1);

  refHits.length = 0;
  const extraSame = click(fakeNode({ kind: 'code', text: 'src/owned.ts', sessionId: 'session-owner' }));
  check(
    'extra matching subscriber does not double-open',
    dispatchMarkdownPathClick(extraSame, ownerHandler) === true && refHits.length === 1,
    JSON.stringify(refHits),
  );

  refHits.length = 0;
  const noOwner = click(fakeNode({ kind: 'code', text: 'src/owned.ts' }));
  check(
    'missing rendered owner is inert before subscribers',
    dispatchMarkdownPathClick(noOwner) === false && refHits.length === 0 && noOwner.defaultPrevented === false,
    JSON.stringify(refHits),
  );

  const appHits: Array<{ path: string; sessionId: string }> = [];
  const appHandler = (path: string, _line: number | undefined, owner: { sessionId: string }) => {
    appHits.push({ path, sessionId: owner.sessionId });
  };
  const appUnsub = subscribeMarkdownOpenPath(appHandler);
  const appOwned = click(fakeNode({ kind: 'code', text: 'src/app.ts', sessionId: 'session-a' }));
  check(
    'App-shaped handler opens the rendered owner session',
    dispatchMarkdownPathClick(appOwned) === true
      && appHits.length === 1
      && appHits[0].path === 'src/app.ts'
      && appHits[0].sessionId === 'session-a',
    JSON.stringify(appHits),
  );
  appHits.length = 0;
  const appNoOwner = click(fakeNode({ kind: 'code', text: 'src/app.ts' }));
  check(
    'App-shaped handler is inert without owner session',
    dispatchMarkdownPathClick(appNoOwner) === false && appHits.length === 0,
    JSON.stringify(appHits),
  );
  appUnsub();

  unsubB();
  refHits.length = 0;
  const afterRef = click(fakeNode({ kind: 'code', text: 'src/owned.ts', sessionId: 'session-owner' }));
  check('last unsub drops handler', dispatchMarkdownPathClick(afterRef) === false && refHits.length === 0);

  const schemeHits: string[] = [];
  const schemeUnsub = subscribeMarkdownOpenPath((path) => { schemeHits.push(path); });
  const schemeEvent = click(fakeNode({ kind: 'anchor', href: 'javascript:alert(1)', sessionId: 'session-a' }));
  check(
    'invalid scheme stays inert with live subscriber',
    dispatchMarkdownPathClick(schemeEvent) === false && schemeHits.length === 0 && schemeEvent.defaultPrevented === false,
  );
  schemeUnsub();

  const html = renderMarkdown('see [safe](https://example.com/file.bin) and [mail](mailto:a@b.c) and [src](src/app.ts)');
  check('https link keeps navigation', html.includes('<a href="https://example.com/file.bin" rel="noopener noreferrer">safe</a>'), html.slice(0, 400));
  check('https link offers Download', html.includes('class="md-link-download" data-href="https://example.com/file.bin"'), html.slice(0, 400));
  check('mailto has no Download', !html.includes('data-href="mailto:a@b.c"'), html.slice(0, 400));
  check('relative workspace href renders', html.includes('<a href="src/app.ts" rel="noopener noreferrer">src</a>'), html.slice(0, 400));
  check('relative href has no Download', !html.includes('data-href="src/app.ts"'), html.slice(0, 400));
  const pathLineHtml = renderMarkdown('open [here](src/app.ts:12) and [file](app.ts:8)');
  check(
    'path:line href still renders',
    pathLineHtml.includes('<a href="src/app.ts:12" rel="noopener noreferrer">here</a>'),
    pathLineHtml.slice(0, 400),
  );
  check(
    'file.ext:line href is not a workspace link',
    !/href=["']?app\.ts:8/i.test(pathLineHtml) && pathLineHtml.includes('file'),
    pathLineHtml.slice(0, 400),
  );
  const schemeHtml = renderMarkdown(
    'see [ftp](ftp://example.com/file.txt) [ssh](ssh://host/src/app.ts) [app](custom-app:open)',
  );
  check('ftp href not re-enabled', !/href=["']?ftp:/i.test(schemeHtml), schemeHtml.slice(0, 400));
  check('ssh href not re-enabled', !/href=["']?ssh:/i.test(schemeHtml), schemeHtml.slice(0, 400));
  check('custom-app href not re-enabled', !/href=["']?custom-app:/i.test(schemeHtml), schemeHtml.slice(0, 400));
  check(
    'rejected schemes render as labels',
    schemeHtml.includes('ftp') && schemeHtml.includes('ssh') && schemeHtml.includes('app')
      && !schemeHtml.includes('<a '),
    schemeHtml.slice(0, 400),
  );
  const numericSchemeHtml = renderMarkdown(
    'see [ftp](ftp:12) [ssh](ssh:12) [app](custom-app:12) [mix](FTP:12) [plus](svn+ssh:12) [dot](app.custom:12)',
  );
  check('ftp:12 href not rendered', !/href=["']?ftp:12/i.test(numericSchemeHtml), numericSchemeHtml.slice(0, 400));
  check('ssh:12 href not rendered', !/href=["']?ssh:12/i.test(numericSchemeHtml), numericSchemeHtml.slice(0, 400));
  check('custom-app:12 href not rendered', !/href=["']?custom-app:12/i.test(numericSchemeHtml), numericSchemeHtml.slice(0, 400));
  check('mixed-case scheme href not rendered', !/href=["']?FTP:12/i.test(numericSchemeHtml), numericSchemeHtml.slice(0, 400));
  check('plus scheme href not rendered', !/href=["']?svn\+ssh:12/i.test(numericSchemeHtml), numericSchemeHtml.slice(0, 400));
  check('dot scheme href not rendered', !/href=["']?app\.custom:12/i.test(numericSchemeHtml), numericSchemeHtml.slice(0, 400));
  check(
    'numeric schemes render as labels',
    numericSchemeHtml.includes('ftp') && numericSchemeHtml.includes('ssh') && numericSchemeHtml.includes('app')
      && numericSchemeHtml.includes('mix') && numericSchemeHtml.includes('plus') && numericSchemeHtml.includes('dot')
      && !numericSchemeHtml.includes('<a '),
    numericSchemeHtml.slice(0, 400),
  );
  const punctSchemeHtml = renderMarkdown(
    'see [ftp](ftp:) [data](data:) [app](custom-app:)',
  );
  check('ftp: punctuation-only href not rendered', !/href=["']?ftp:/i.test(punctSchemeHtml), punctSchemeHtml.slice(0, 400));
  check('data: punctuation-only href not rendered', !/href=["']?data:/i.test(punctSchemeHtml), punctSchemeHtml.slice(0, 400));
  check('custom-app: punctuation-only href not rendered', !/href=["']?custom-app:/i.test(punctSchemeHtml), punctSchemeHtml.slice(0, 400));
  check(
    'punctuation-only schemes render as labels',
    punctSchemeHtml.includes('ftp') && punctSchemeHtml.includes('data') && punctSchemeHtml.includes('app')
      && !punctSchemeHtml.includes('<a '),
    punctSchemeHtml.slice(0, 400),
  );
  const codePathHtml = renderMarkdown('open `src/Makefile:12` and `web/app.ts:42`');
  check(
    'Makefile:12 becomes a path link',
    codePathHtml.includes('class="md-code md-code-path"')
      && codePathHtml.includes('data-md-path="src/Makefile"')
      && codePathHtml.includes('data-md-line="12"')
      && codePathHtml.includes('>src/Makefile:12</code>'),
    codePathHtml.slice(0, 400),
  );
  check(
    'app.ts:42 becomes a path link',
    codePathHtml.includes('data-md-path="web/app.ts"')
      && codePathHtml.includes('data-md-line="42"')
      && codePathHtml.includes('>web/app.ts:42</code>'),
    codePathHtml.slice(0, 400),
  );
  const citedHtml = renderMarkdown('see `src/file.rs:299` and `not a path` and `src/app.ts`');
  check(
    'file.rs:299 matches a path:line link',
    citedHtml.includes('data-md-path="src/file.rs"')
      && citedHtml.includes('data-md-line="299"')
      && citedHtml.includes('class="md-code md-code-path"')
      && citedHtml.includes('>src/file.rs:299</code>'),
    citedHtml,
  );
  check(
    'unmatched inline code stays an ordinary span',
    citedHtml.includes('<code class="md-code">not a path</code>')
      && !citedHtml.includes('data-md-path="not a path"'),
    citedHtml,
  );
  const appCode = citedHtml.match(/<code class="md-code[^"]*" [^>]*>src\/app\.ts<\/code>/)?.[0] ?? '';
  check(
    'no-line directory path is a path link without a line',
    appCode.includes('data-md-path="src/app.ts"')
      && !appCode.includes('data-md-line=')
      && !appCode.includes('data-md-resolved='),
    appCode || citedHtml,
  );
  const grepContext = 'src/runtime/process_block.rs-472-    fn handle(\nsrc/runtime/process_block.rs-480-    }';
  const completedBasenameHtml = renderMarkdown('see `process_block.rs:472-480`', grepContext);
  check(
    'unique nearby grep path is the open path',
    completedBasenameHtml.includes('class="md-code md-code-path"')
      && completedBasenameHtml.includes('data-md-path="src/runtime/process_block.rs"')
      && !completedBasenameHtml.includes('data-md-completion=')
      && completedBasenameHtml.includes('data-md-line="472"')
      && completedBasenameHtml.includes('>process_block.rs:472-480</code>'),
    completedBasenameHtml,
  );
  const homonymHtml = renderMarkdown(
    'see `process_block.rs:472-480`',
    'src/runtime/process_block.rs and crates/other/process_block.rs',
  );
  check(
    'homonym basename cite stays unresolved',
    homonymHtml.includes('data-md-resolved="none"')
      && homonymHtml.includes('>process_block.rs:472-480</code>')
      && !homonymHtml.includes('md-code-path'),
    homonymHtml,
  );
  {
    const citation = '`gatherer.rs:178-260` [source](gatherer.rs) `src/direct.rs:9`';
    const direct = '<code class="md-code md-code-path" data-md-path="src/direct.rs" data-md-line="9">src/direct.rs:9</code>';
    const contexts: Array<{ name: string; text: string; path: string | null }> = [
      { name: 'unique', text: 'src/queue/gatherer.rs:178:fn gather_work() { // queue scan\n', path: 'src/queue/gatherer.rs' },
      { name: 'ambiguous', text: 'src/queue/gatherer.rs legacy/gatherer.rs', path: null },
      { name: 'changed owner context', text: 'other/workspace/gatherer.rs', path: 'other/workspace/gatherer.rs' },
      { name: 'unique again', text: 'src/queue/gatherer.rs', path: 'src/queue/gatherer.rs' },
    ];
    for (const context of contexts) {
      const code = context.path
        ? `<code class="md-code md-code-path" data-md-path="${context.path}" data-md-line="178">gatherer.rs:178-260</code>`
        : '<code class="md-code" data-md-resolved="none">gatherer.rs:178-260</code>';
      const expected = `<p class="md-p">${code} source ${direct}</p>`;
      for (const render of [renderMarkdown, renderStreamingMarkdown]) {
        const indexOf = String.prototype.indexOf;
        let scans = 0;
        let singleScans = 0;
        let singleHtml = '';
        let repeatedHtml = '';
        // Count only searches of this exact context; restore before any assertion.
        String.prototype.indexOf = function (search: string, from?: number): number {
          if (String(this) === context.text && search === 'gatherer.rs') scans++;
          return indexOf.call(this, search, from);
        };
        try {
          singleHtml = render(citation, context.text);
          singleScans = scans;
          scans = 0;
          repeatedHtml = render(`${citation}\n`.repeat(64), context.text);
        } finally {
          String.prototype.indexOf = indexOf;
        }
        check(`${render.name}: ${context.name} exact single citation output`, singleHtml === expected, singleHtml);
        check(`${render.name}: ${context.name} repeated citation output`, repeatedHtml === expected.repeat(64));
        check(`${render.name}: ${context.name} context search work does not grow with citations`,
          scans <= singleScans, `single=${singleScans}, repeated=${scans}`);
      }
    }
  }
  const chineseHtml = renderMarkdown('见 `文档/文件.rs:299`');
  check(
    'Chinese path:line becomes a path link',
    chineseHtml.includes('data-md-path="文档/文件.rs"')
      && chineseHtml.includes('data-md-line="299"')
      && chineseHtml.includes('>文档/文件.rs:299</code>'),
    chineseHtml,
  );
  const absCiteHtml = renderMarkdown('open `/workspace/project/src/file.rs:299`');
  check(
    'workspace-absolute path:line becomes a path link',
    absCiteHtml.includes('data-md-path="/workspace/project/src/file.rs"')
      && absCiteHtml.includes('data-md-line="299"'),
    absCiteHtml,
  );
  const fragmentHtml = renderMarkdown('see [jump](#section) and [bare](#)');
  check('hash fragment keeps navigation', fragmentHtml.includes('<a href="#section" rel="noopener noreferrer">jump</a>'), fragmentHtml.slice(0, 400));
  check('bare hash keeps navigation', fragmentHtml.includes('<a href="#" rel="noopener noreferrer">bare</a>'), fragmentHtml.slice(0, 400));
  check('hash fragment has no Download', !fragmentHtml.includes('data-href="#section"') && !fragmentHtml.includes('data-href="#"'), fragmentHtml.slice(0, 400));

  const e2eAbs = '/workspace/rpi-feat-test/long.txt';
  const e2eUrl = 'https://example.com/notes.md';
  const e2eHtml = renderMarkdown(`open ${e2eAbs} and ${e2eUrl}`);
  check(
    'e2e bare absolute workspace path stays plain prose (not an anchor)',
    !e2eHtml.includes('md-bare-abs')
      && e2eHtml.includes(`open ${e2eAbs} and `)
      && !e2eHtml.includes(`href="${e2eAbs}"`),
    e2eHtml.slice(0, 600),
  );
  check(
    'e2e bare https is interactive with Download',
    e2eHtml.includes(`<a href="${e2eUrl}" rel="noopener noreferrer">${e2eUrl}</a>`)
      && e2eHtml.includes(`class="md-link-download" data-href="${e2eUrl}"`),
    e2eHtml.slice(0, 600),
  );

  const punctHtml = renderMarkdown(`see ${e2eAbs}. also ${e2eUrl}.`);
  check(
    'bare absolute path stays literal text with its punctuation',
    punctHtml.includes(`${e2eAbs}. also`)
      && !punctHtml.includes('md-bare-abs')
      && !punctHtml.includes('</a> '),
    punctHtml.slice(0, 500),
  );
  check(
    'https punctuation stays outside the target',
    punctHtml.includes(`${e2eUrl}</a>`) && punctHtml.includes('Download</button>.'),
    punctHtml.slice(0, 500),
  );

  const multiHtml = renderMarkdown(`a ${e2eAbs} b ${e2eUrl} c /workspace/other.txt`);
  check(
    'bare absolute paths stay prose while bare https linkifies',
    !multiHtml.includes('md-bare-abs')
      && multiHtml.includes(e2eAbs)
      && multiHtml.includes('/workspace/other.txt')
      && (multiHtml.match(/class="md-link-download"/g) ?? []).length === 1,
    multiHtml.slice(0, 700),
  );

  const existingLinkHtml = renderMarkdown(`keep [safe](${e2eUrl}) plus ${e2eUrl}`);
  check(
    'existing markdown https is not double-linked',
    (existingLinkHtml.match(/<a href="https:\/\/example\.com\/notes\.md"/g) ?? []).length === 2
      && (existingLinkHtml.match(/class="md-link-download" data-href="https:\/\/example\.com\/notes\.md"/g) ?? []).length === 2
      && !/<a [^>]*>[^<]*<a /.test(existingLinkHtml),
    existingLinkHtml.slice(0, 600),
  );
  const existingPathHtml = renderMarkdown('see [here](src/app.ts) and `src/app.ts`');
  check(
    'existing relative link and code stay unduplicated',
    existingPathHtml.includes('<a href="src/app.ts" rel="noopener noreferrer">here</a>')
      && existingPathHtml.includes('data-md-path="src/app.ts"')
      && existingPathHtml.includes('>src/app.ts</code>')
      && (existingPathHtml.match(/href="src\/app.ts"/g) ?? []).length === 1,
    existingPathHtml.slice(0, 500),
  );

  const codeBareHtml = renderMarkdown('keep `https://example.com/notes.md` and `/workspace/rpi-feat-test/long.txt`');
  check(
    'bare tokens inside code stay code',
    codeBareHtml.includes('<code class="md-code">https://example.com/notes.md</code>')
      && codeBareHtml.includes('data-md-path="/workspace/rpi-feat-test/long.txt"')
      && codeBareHtml.includes('>/workspace/rpi-feat-test/long.txt</code>')
      && !codeBareHtml.includes('md-bare-abs')
      && !codeBareHtml.includes('md-link-download'),
    codeBareHtml.slice(0, 500),
  );
  const fenceBareHtml = renderMarkdown('```\nhttps://example.com/notes.md\n/workspace/rpi-feat-test/long.txt\n```');
  check(
    'bare tokens inside fences stay fences',
    fenceBareHtml.includes('class="md-fence"')
      && fenceBareHtml.includes('https://example.com/notes.md')
      && fenceBareHtml.includes('/workspace/rpi-feat-test/long.txt')
      && !fenceBareHtml.includes('md-bare-abs')
      && !fenceBareHtml.includes('md-link-download'),
    fenceBareHtml.slice(0, 500),
  );

  const xssBareHtml = renderMarkdown('go javascript:alert(1) and <script>alert(2)</script> and ftp://example.com/x.txt');
  check(
    'bare XSS and generic schemes stay inert',
    xssBareHtml.includes('&lt;script&gt;alert(2)&lt;/script&gt;')
      && !xssBareHtml.toLowerCase().includes('href="javascript:')
      && !/href=["']?ftp:/i.test(xssBareHtml)
      && !xssBareHtml.includes('<script'),
    xssBareHtml.slice(0, 500),
  );
  const passwdHtml = renderMarkdown('do not open /etc/passwd or / or /workspace/../secret.txt');
  check(
    '/etc/passwd root and traversal stay inert',
    !passwdHtml.includes('md-bare-abs')
      && passwdHtml.includes('/etc/passwd')
      && passwdHtml.includes('/workspace/../secret.txt'),
    passwdHtml.slice(0, 500),
  );

  // --- strict file_explorer_read validation gate (markdownOpenValidatedPath) ---

  const validRead = (path: string) => ({
    path,
    lines: ['first line'],
    totalLines: 1,
    sizeBytes: 11,
    truncated: false,
  });

  {
    interface probeCall { command: Record<string, unknown> }
    const probes: probeCall[] = [];
    const opens: Array<{ path: string; line?: number; sessionId: string; workspaceCwd?: string }> = [];
    const errors: unknown[] = [];
    const open = (path: string, line: number | undefined, owner: { sessionId: string; workspaceCwd?: string }) => {
      opens.push({ path, line, ...owner });
    };
    const onError = (error: unknown) => { errors.push(error); };

    // Valid owner probe: exact session + path + maxLines 1 → opens, zero errors.
    probes.length = 0;
    opens.length = 0;
    errors.length = 0;
    await markdownOpenValidatedPath(
      'src/lib.rs',
      3,
      { sessionId: 'session-a', workspaceCwd: '/workspace/project-a' },
      (command) => {
        probes.push({ command });
        return Promise.resolve(validRead('src/lib.rs'));
      },
      open,
      onError,
    );
    check('valid probe opens', probes.length === 1
      && probes[0].command.type === 'file_explorer_read'
      && probes[0].command.path === 'src/lib.rs'
      && probes[0].command.sessionId === 'session-a'
      && probes[0].command.maxLines === 1
      && opens.length === 1
      && opens[0].path === 'src/lib.rs'
      && opens[0].line === 3
      && opens[0].sessionId === 'session-a'
      && errors.length === 0,
      JSON.stringify({ probes, opens, errors }));

    // Send rejection is user-owned: the error owner fires EXACTLY once and
    // the open never happens — plain and transport-shaped alike.
    probes.length = 0;
    opens.length = 0;
    errors.length = 0;
    await markdownOpenValidatedPath(
      'src/missing.ts',
      undefined,
      { sessionId: 'session-a', workspaceCwd: '/workspace/project-a' },
      () => Promise.reject(new Error('file is unavailable')),
      open,
      onError,
    );
    check('send rejection reaches the error owner exactly once and never opens',
      opens.length === 0
        && errors.length === 1
        && errors[0] instanceof Error
        && (errors[0] as Error).message === 'file is unavailable',
      JSON.stringify({ opens, errors }));

    opens.length = 0;
    errors.length = 0;
    await markdownOpenValidatedPath(
      'src/lib.rs',
      undefined,
      { sessionId: 'session-a', workspaceCwd: '/workspace/project-a' },
      () => Promise.reject(Object.assign(new Error('connection lost'), { transport: true })),
      open,
      onError,
    );
    check('transport-shaped rejection reaches the error owner exactly once',
      opens.length === 0 && errors.length === 1,
      JSON.stringify({ opens, errors }));

    // Promise.reject(null/undefined) must reach the error owner exactly
    // once and must never throw out of the gate itself.
    opens.length = 0;
    errors.length = 0;
    await markdownOpenValidatedPath(
      'src/lib.rs',
      undefined,
      { sessionId: 'session-a', workspaceCwd: '/workspace/project-a' },
      () => Promise.reject(null),
      open,
      onError,
    );
    check('null rejection reaches the error owner exactly once', opens.length === 0 && errors.length === 1 && errors[0] === null, JSON.stringify({ opens, errors }));
    opens.length = 0;
    errors.length = 0;
    await markdownOpenValidatedPath(
      'src/lib.rs',
      undefined,
      { sessionId: 'session-a', workspaceCwd: '/workspace/project-a' },
      () => Promise.reject(undefined),
      open,
      onError,
    );
    check('undefined rejection reaches the error owner exactly once', opens.length === 0 && errors.length === 1 && errors[0] === undefined, JSON.stringify({ opens, errors }));

    // Malformed payload is not success: the click is answered with the
    // typed missing-file verdict, never dropped silently.
    probes.length = 0;
    opens.length = 0;
    errors.length = 0;
    await markdownOpenValidatedPath(
      'src/lib.rs',
      undefined,
      { sessionId: 'session-a', workspaceCwd: '/workspace/project-a' },
      (command) => {
        probes.push({ command });
        return Promise.resolve({ nonsense: true });
      },
      open,
      onError,
    );
    check(
      'malformed read response opens nothing and reports the typed missing-file verdict',
      opens.length === 0
        && probes.length === 1
        && errors.length === 1
        && rejectionFields(errors[0]).rpc === true
        && rejectionFields(errors[0]).rpcCode === 'file_unavailable'
        && markdownOpenRejectionOwnership(errors[0]) === 'local'
        && markdownOpenMissingFileVerdict(true, errors[0]) === true,
      JSON.stringify({ opens, errors }),
    );

    // Returned path identity: an unrelated echoed path cannot be success —
    // the same typed verdict, one error, no open.
    opens.length = 0;
    errors.length = 0;
    await markdownOpenValidatedPath(
      'src/lib.rs',
      undefined,
      { sessionId: 'session-a', workspaceCwd: '/workspace/project-a' },
      () => Promise.resolve(validRead('src/other.ts')),
      open,
      onError,
    );
    check(
      'mismatched returned path rejects the open with the typed missing-file verdict',
      opens.length === 0
        && errors.length === 1
        && rejectionFields(errors[0]).rpcCode === 'file_unavailable',
      JSON.stringify({ opens, errors }),
    );

    opens.length = 0;
    errors.length = 0;
    await markdownOpenValidatedPath(
      'missing.rs',
      299,
      { sessionId: 'session-a', workspaceCwd: '/workspace/project-a' },
      () => Promise.resolve(validRead('src/other.ts')),
      open,
      onError,
    );
    check(
      'missing path:line probe never opens and names the path in its toast',
      opens.length === 0
        && errors.length === 1
        && markdownOpenMissingFileVerdict(true, errors[0]) === true
        && markdownOpenMissingFileToast('missing.rs') === '文件已不存在：missing.rs',
      JSON.stringify({ opens, errors }),
    );

    // Absolute candidates still bind to the owner workspace cwd first.
    probes.length = 0;
    opens.length = 0;
    errors.length = 0;
    await markdownOpenValidatedPath(
      '/workspace/project-a/src/lib.rs',
      undefined,
      { sessionId: 'session-a', workspaceCwd: '/workspace/project-a' },
      (command) => {
        probes.push({ command });
        return Promise.resolve(validRead('src/lib.rs'));
      },
      open,
      onError,
    );
    check(
      'absolute descendant probes the relative form and opens relative',
      probes.length === 1 && probes[0].command.path === 'src/lib.rs'
        && opens.length === 1 && opens[0].path === 'src/lib.rs'
        && opens[0].sessionId === 'session-a'
        && errors.length === 0,
      JSON.stringify({ probes, opens, errors }),
    );
    // Invalid candidates (outside cwd, untrimmed) error nothing and open nothing.
    probes.length = 0;
    opens.length = 0;
    errors.length = 0;
    await markdownOpenValidatedPath(
      '/workspace/other/secret.ts',
      undefined,
      { sessionId: 'session-a', workspaceCwd: '/workspace/project-a' },
      (command) => {
        probes.push({ command });
        return Promise.resolve(validRead('src/other.ts'));
      },
      open,
      onError,
    );
    check('outside-cwd absolute never probes and never opens', opens.length === 0 && probes.length === 0 && errors.length === 0);
    opens.length = 0;
    errors.length = 0;
    await markdownOpenValidatedPath(
      ' src/lib.rs ',
      undefined,
      { sessionId: 'session-a', workspaceCwd: '/workspace/project-a' },
      (command) => {
        probes.push({ command });
        return Promise.resolve(validRead('src/lib.rs'));
      },
      open,
      onError,
    );
    check('untrimmed invalid path never probes, never opens, never errors', opens.length === 0 && probes.length === 0 && errors.length === 0);

    check('normalizeRead success on single-line read', markdownPathReadValid('src/lib.rs', validRead('src/lib.rs')) === true);
    check('mismatched echoed path rejects', markdownPathReadValid('src/lib.rs', validRead('src/other.ts')) === false);
    check('malformed response rejects', markdownPathReadValid('src/lib.rs', null) === false);
    check('empty response path rejects', markdownPathReadValid('src/lib.rs', { path: '', lines: ['x'], totalLines: 1, sizeBytes: 1, truncated: false }) === false);
  }

  {
    // Probe-to-open identity: a single-segment basename request whose
    // probe echoes the server's unique-basename RESOLUTION must open the
    // RESOLVED deeper path — never the request basename — so the drawer's
    // pagination reads the established identity, not repeated basename
    // suffix-matches that could mix homonym content.
    const probes: Array<Record<string, unknown>> = [];
    const opens: Array<string> = [];
    const errors: unknown[] = [];
    const resolved = 'psy_node_common/src/realm/processor/db/init.rs';
    await markdownOpenValidatedPath(
      'init.rs',
      299,
      { sessionId: 'session-a', workspaceCwd: '/workspace/project-a' },
      (command) => {
        probes.push(command);
        return Promise.resolve(validRead(resolved));
      },
      (opened, line) => { opens.push(`${opened}:${line ?? ''}`); },
      (error) => { errors.push(error); },
    );
    check('probe resolves a basename request to the deeper echoed path',
      probes.length === 1 && opens.length === 1 && opens[0] === `${resolved}:299` && errors.length === 0,
      JSON.stringify({ probes: probes.length, opens, errors: errors.length }));
  }

  {
    const echo = (echoPath: string) => ({
      path: echoPath,
      lines: ['ok'],
      totalLines: 1,
      sizeBytes: 8,
      truncated: false,
    });
    const probes: Array<Record<string, unknown>> = [];
    const opens: string[] = [];
    const errors: unknown[] = [];
    await markdownOpenValidatedPath(
      'pkg/nested/target.rs',
      1,
      { sessionId: 'session-a', workspaceCwd: '/workspace' },
      (command) => {
        probes.push(command);
        return Promise.resolve(echo('pkg/nested/target.rs'));
      },
      (opened, line) => { opens.push(`${opened}:${line ?? ''}`); },
      (error) => { errors.push(error); },
      'vendor/pkg/nested/target.rs',
    );
    check(
      'exact read success opens the cited path and sends no alternate read',
      probes.length === 1
        && probes[0].path === 'pkg/nested/target.rs'
        && opens.join(',') === 'pkg/nested/target.rs:1'
        && errors.length === 0,
      JSON.stringify({ probes, opens, errors }),
    );
    probes.length = 0;
    opens.length = 0;
    errors.length = 0;
    await markdownOpenValidatedPath(
      'nested/target.rs',
      1,
      { sessionId: 'session-a', workspaceCwd: '/workspace' },
      (command) => {
        probes.push(command);
        if (command.path === 'nested/target.rs') {
          const error = new Error('file is unavailable') as Error & { rpcCode: 'file_unavailable' };
          error.rpcCode = 'file_unavailable';
          return Promise.reject(error);
        }
        return Promise.resolve(echo('pkg/nested/target.rs'));
      },
      (opened, line) => { opens.push(`${opened}:${line ?? ''}`); },
      (error) => { errors.push(error); },
      'pkg/nested/target.rs',
    );
    check(
      'typed missing file then reads the unique completion once',
      probes.length === 2
        && probes[0].path === 'nested/target.rs'
        && probes[1].path === 'pkg/nested/target.rs'
        && opens.join(',') === 'pkg/nested/target.rs:1'
        && errors.length === 0,
      JSON.stringify({ probes, opens, errors }),
    );
  }

  {
    const rootProbes: Array<Record<string, unknown>> = [];
    const rootOpens: string[] = [];
    const rootErrors: unknown[] = [];
    let rootProbe: Promise<void> = Promise.resolve();
    const rootHandler = (
      path: string,
      line: number | undefined,
      owner: { sessionId: string; workspaceCwd?: string },
      completion?: string,
    ) => {
      rootProbe = markdownOpenValidatedPath(
        path,
        line,
        owner,
        (command) => {
          rootProbes.push(command);
          if (command.path === 'foo.rs') return Promise.resolve(validRead('foo.rs'));
          return Promise.resolve(validRead('src/foo.rs'));
        },
        (opened, openLine) => { rootOpens.push(`${opened}:${openLine ?? ''}`); },
        (error) => { rootErrors.push(error); },
        completion,
      );
    };
    const rootHtml = renderMarkdown('`foo.rs`', 'the file is src/foo.rs');
    const rootPath = rootHtml.match(/data-md-path="([^"]+)"/)?.[1];
    const rootEvent = click(fakeNode({
      kind: 'code',
      text: 'foo.rs',
      dataMdPath: rootPath,
      sessionId: 'session-a',
      workspaceCwd: '/workspace',
      rootText: 'the file is foo.rs src/foo.rs glued',
    }));
    check(
      'bare foo.rs beside src/foo.rs is a click',
      dispatchMarkdownPathClick(rootEvent, rootHandler) === true,
    );
    await rootProbe;
    check(
      'bare foo.rs probes only the unique contextual path, never a root basename',
      rootProbes.length === 1
        && rootProbes[0].path === 'src/foo.rs'
        && !rootProbes.some((command) => command.path === 'foo.rs')
        && rootOpens.join(',') === 'src/foo.rs:'
        && rootErrors.length === 0,
      JSON.stringify({ rootProbes, rootOpens, rootErrors }),
    );
    check(
      'two different longer paths for the same basename are not a click',
      dispatchMarkdownPathClick(click(fakeNode({
        kind: 'code',
        text: 'foo.rs',
        dataMdResolved: 'none',
        sessionId: 'session-a',
        rootText: 'src/foo.rs and lib/foo.rs',
      })), rootHandler) === false,
    );

  }

  {
    const glued = 'duplicate dup-name.txt:1 beside pkg/a/dup-name.txt and pkg/b/dup-name.txtoutside /tmp/work/outside-only.txt:1';
    const dupClick = click(fakeNode({
      kind: 'code',
      text: 'dup-name.txt:1',
      dataMdResolved: 'none',
      sessionId: 'session-a',
      rootText: glued,
    }));
    check(
      'rendered duplicate stays inert when paragraph text glues the second path',
      dispatchMarkdownPathClick(dupClick, onOpen) === false && dupClick.defaultPrevented === false,
    );
  }
  {
    // Regression: a root basename with no longer context path (empty context
    // or raw-only citation, with/without line) must stamp the exact
    // candidate; two longer context paths stay render-unresolved and inert;
    // the unique-context stamp is unchanged. Clicks use actual rendered attrs.
    const renderedCodeTag = (html: string, inner: string): { cls: string; attrs: Record<string, string> } | null => {
      const pattern = new RegExp(`<code class="(md-code[^"]*)"([^>]*)>${inner.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}</code>`);
      const m = html.match(pattern);
      if (!m) return null;
      const attrs: Record<string, string> = {};
      const attrRe = /(data-md-[a-z]+)="([^"]*)"/g;
      let a: RegExpExecArray | null;
      while ((a = attrRe.exec(m[2])) !== null) attrs[a[1]] = a[2];
      return { cls: m[1], attrs };
    };
    const rootOpens: Array<{ path: string; line?: number; sessionId: string; workspaceCwd?: string; completion?: string }> = [];
    const onRootOpen = (
      openedPath: string,
      openedLine: number | undefined,
      owner: { sessionId: string; workspaceCwd?: string },
      completion?: string,
    ) => {
      rootOpens.push({ path: openedPath, line: openedLine, ...owner, completion });
    };

    const lineHtml = renderMarkdown('see `preview.md:2` for details');
    const lineTag = renderedCodeTag(lineHtml, 'preview.md:2');
    check(
      'root basename:line with raw-only context stamps the exact candidate code tag',
      lineTag !== null
        && lineTag.cls === 'md-code md-code-path'
        && lineTag.attrs['data-md-path'] === 'preview.md'
        && lineTag.attrs['data-md-line'] === '2'
        && !('data-md-resolved' in lineTag.attrs),
      lineHtml.slice(0, 300),
    );
    const lineStreamHtml = renderStreamingMarkdown('see `preview.md:2` for details');
    const lineStreamTag = renderedCodeTag(lineStreamHtml, 'preview.md:2');
    check(
      'streaming render stamps the same root basename:line candidate',
      lineStreamTag !== null
        && lineStreamTag.attrs['data-md-path'] === 'preview.md'
        && lineStreamTag.attrs['data-md-line'] === '2'
        && lineStreamTag.attrs['data-md-resolved'] === undefined,
      lineStreamHtml.slice(0, 300),
    );

    const bareHtml = renderMarkdown('check `Token.sol` first');
    const bareTag = renderedCodeTag(bareHtml, 'Token.sol');
    check(
      'root basename without a line stamps the exact filename candidate',
      bareTag !== null
        && bareTag.cls === 'md-code md-code-path'
        && bareTag.attrs['data-md-path'] === 'Token.sol'
        && !('data-md-line' in bareTag.attrs)
        && !('data-md-resolved' in bareTag.attrs),
      bareHtml.slice(0, 300),
    );
    const emptyCtxHtml = renderMarkdown('`Token.sol`', '');
    const emptyCtxTag = renderedCodeTag(emptyCtxHtml, 'Token.sol');
    check(
      'empty path context still stamps the exact root filename candidate',
      emptyCtxTag !== null
        && emptyCtxTag.attrs['data-md-path'] === 'Token.sol'
        && emptyCtxTag.attrs['data-md-resolved'] === undefined,
      emptyCtxHtml.slice(0, 300),
    );

    const ambiguousHtml = renderMarkdown('`Token.sol` beside a/Token.sol and b/Token.sol');
    const ambiguousTag = renderedCodeTag(ambiguousHtml, 'Token.sol');
    check(
      'two longer context paths keep the basename render-unresolved',
      ambiguousTag !== null
        && ambiguousTag.cls === 'md-code'
        && ambiguousTag.attrs['data-md-resolved'] === 'none'
        && !('data-md-path' in ambiguousTag.attrs)
        && !('data-md-line' in ambiguousTag.attrs),
      ambiguousHtml.slice(0, 300),
    );
    const uniqueHtml = renderMarkdown('`Token.sol` beside src/Token.sol');
    const uniqueTag = renderedCodeTag(uniqueHtml, 'Token.sol');
    check(
      'unique longer context keeps the completed stamp unchanged',
      uniqueTag !== null
        && uniqueTag.cls === 'md-code md-code-path'
        && uniqueTag.attrs['data-md-path'] === 'src/Token.sol'
        && uniqueTag.attrs['data-md-resolved'] === undefined,
      uniqueHtml.slice(0, 300),
    );

    rootOpens.length = 0;
    const stampedEvent = click(fakeNode({
      kind: 'code',
      text: 'preview.md:2',
      dataMdPath: lineTag?.attrs['data-md-path'] ?? null,
      dataMdLine: lineTag?.attrs['data-md-line'] ?? null,
      dataMdResolved: lineTag?.attrs['data-md-resolved'] ?? null,
      sessionId: 'session-root',
      workspaceCwd: '/workspace',
    }));
    check(
      'root basename:line click dispatches the exact stamped candidate to its owner',
      dispatchMarkdownPathClick(stampedEvent, onRootOpen) === true
        && stampedEvent.defaultPrevented === true
        && rootOpens.length === 1
        && rootOpens[0].path === 'preview.md'
        && rootOpens[0].line === 2
        && rootOpens[0].sessionId === 'session-root'
        && rootOpens[0].completion === undefined,
      JSON.stringify(rootOpens),
    );
    rootOpens.length = 0;
    const bareStampedEvent = click(fakeNode({
      kind: 'code',
      text: 'Token.sol',
      dataMdPath: bareTag?.attrs['data-md-path'] ?? null,
      dataMdLine: bareTag?.attrs['data-md-line'] ?? null,
      dataMdResolved: bareTag?.attrs['data-md-resolved'] ?? null,
      sessionId: 'session-root',
      workspaceCwd: '/workspace',
    }));
    check(
      'root basename without line dispatches the exact filename, no guessed completion',
      dispatchMarkdownPathClick(bareStampedEvent, onRootOpen) === true
        && rootOpens.length === 1
        && rootOpens[0].path === 'Token.sol'
        && rootOpens[0].line === undefined
        && rootOpens[0].sessionId === 'session-root'
        && rootOpens[0].completion === undefined,
      JSON.stringify(rootOpens),
    );
    rootOpens.length = 0;
    const ambiguousEvent = click(fakeNode({
      kind: 'code',
      text: 'Token.sol',
      dataMdPath: ambiguousTag?.attrs['data-md-path'] ?? null,
      dataMdResolved: ambiguousTag?.attrs['data-md-resolved'] ?? null,
      sessionId: 'session-root',
    }));
    check(
      'ambiguous-context basename click stays inert from its rendered attributes',
      dispatchMarkdownPathClick(ambiguousEvent, onRootOpen) === false
        && rootOpens.length === 0
        && ambiguousEvent.defaultPrevented === false,
    );
  }
  {
    // preventDefault fires synchronously for explicit owner candidates even
    // though opening now awaits async validation.
    const seen: string[] = [];
    const unsub = subscribeMarkdownOpenPath((path) => { seen.push(path); });
    const codeEvent = click(fakeNode({ kind: 'code', text: 'src/app.ts', sessionId: 'session-a' }));
    check(
      'preventDefault stays synchronous for the owner candidate',
      dispatchMarkdownPathClick(codeEvent, onOpen) === true && codeEvent.defaultPrevented === true,
    );
    unsub();
  }

  opened.length = 0;
  const blockedDl = click(fakeNode({ kind: 'download', dataHref: 'javascript:alert(1)' }));
  check(
    'download click is explicit and blocked for non-http',
    dispatchMarkdownPathClick(blockedDl, onOpen) === true
      && blockedDl.defaultPrevented === true
      && opened.length === 0,
  );

  check(
    'download filename from URL',
    markdownDownloadFileName('https://example.com/path/report.bin') === 'report.bin',
  );
  const scrubbed = markdownDownloadFileName('https://example.com/a', 'attachment; filename="a/b\\c.txt"');
  check('download filename bounded scrub', scrubbed === 'a-b-c.txt', scrubbed);
  check('download filename fallback', markdownDownloadFileName('https://example.com/') === 'download');
}

// ---- toast stacking: above footer, no layout shift ----
{
  const css = fs.readFileSync(path.join(process.cwd(), 'src/styles.css'), 'utf8');
  const toastRule = css.match(/#toasts\s*\{([^}]+)\}/);
  const footerRule = css.match(/^footer\s*\{([^}]+)\}/m);
  const toastZ = Number(toastRule?.[1].match(/z-index:\s*(\d+)/)?.[1] ?? 0);
  const footerZ = Number(footerRule?.[1].match(/z-index:\s*(\d+)/)?.[1] ?? 0);
  check('toast layer is position:fixed (no layout shift)', /position:\s*fixed/.test(toastRule?.[1] ?? ''));
  check(
    'toast z-index sits above the composer footer',
    toastZ > footerZ && toastZ >= 53,
    `toasts=${toastZ} footer=${footerZ}`,
  );
  const mobileFooter = css.match(/\.app-layout--drawer-open footer\s*\{([^}]+)\}/);
  const mobileFooterZ = Number(mobileFooter?.[1].match(/z-index:\s*(\d+)/)?.[1] ?? footerZ);
  check(
    'toast z-index sits above the mobile drawer-open footer',
    toastZ > mobileFooterZ,
    `toasts=${toastZ} mobileFooter=${mobileFooterZ}`,
  );
  check(
    'mobile toast rule does not drop z-index below the footer',
    !/@media \(max-width: 768px\)[\s\S]*?#toasts\s*\{[^}]*z-index:\s*(?:[0-4]?\d|5[0-2])\b/.test(css),
  );
  const crToastRule = css.match(/\.app-layout--code-review ~ #toasts\s*\{([^}]+)\}/);
  check(
    'code-review toasts pin with --code-review-toast-top',
    /var\(--code-review-toast-top/.test(crToastRule?.[1] ?? ''),
    crToastRule?.[1] ?? 'missing rule',
  );
  check(
    'code-review toasts are not vertically centered',
    !/translateY\(\s*-50%/.test(crToastRule?.[1] ?? ''),
    crToastRule?.[1] ?? 'missing rule',
  );
  const pathLinkRule = css.match(/\.md-code\.md-code-path\s*\{([^}]+)\}/);
  check(
    'path:line code uses file-link accent underline and keeps copy',
    /color:\s*var\(--accent\)/.test(pathLinkRule?.[1] ?? '')
      && /text-decoration:\s*underline/.test(pathLinkRule?.[1] ?? '')
      && /cursor:\s*pointer/.test(pathLinkRule?.[1] ?? '')
      && /user-select:\s*text/.test(pathLinkRule?.[1] ?? ''),
    pathLinkRule?.[1] ?? 'missing rule',
  );
  check(
    'phone path:line code is a 44px touch target',
    /@media \(max-width: 720px\)[\s\S]*?\.md-code\.md-code-path\s*\{[^}]*min-height:\s*44px/.test(css),
  );
}

{
  const tableMd = [
    '| 级 | 项 | 说明 |',
    '| --- | --- | --- |',
    '| P0 | 唤醒 | 冻结 |',
    '| P1 | 锁 | 作用域 |',
    '| P2 | UI | 批次 |',
  ].join('\n');
  const html = renderMarkdown(tableMd);
  check('priority table renders md-table cells',
    html.includes('class="md-table-wrap"')
      && html.includes('class="md-table"')
      && ASCII_PRIORITY_TOKENS.every((token) => html.includes(`<td>${token}</td>`)));
  check('priority table does not split ASCII tokens in markup',
    !html.includes('>P</') && !html.includes('>P<br'));
  const tableCss = fs.readFileSync(path.join(process.cwd(), 'src/styles.css'), 'utf8');
  const policy = tableCellWrapPolicy(mdTableCellRule(tableCss));
  const broken = phonePriorityColumnContentPx({
    viewportWidth: PHONE_TABLE_VIEWPORT,
    transcriptPadX: 12,
    columns: 6,
    cellPadX: 10,
    borderPx: 1,
    charPx: 8,
    wordBreak: 'break-all',
    overflowWrap: 'anywhere',
    priorityToken: 'P0',
  });
  const anywhereCol = phonePriorityColumnContentPx({
    viewportWidth: PHONE_TABLE_VIEWPORT,
    transcriptPadX: 12,
    columns: 6,
    cellPadX: 10,
    borderPx: 1,
    charPx: 8,
    wordBreak: 'normal',
    overflowWrap: 'anywhere',
    priorityToken: 'P0',
  });
  check('390 markdown table CSS keeps ASCII tokens intact',
    shippedTableCellsKeepAsciiTokens(policy)
      && policy.overflowWrap === 'break-word');
  check('390 break-all would split P0/P1/P2',
    ASCII_PRIORITY_TOKENS.every((token) => asciiTokenLineCount(token, broken, 8, {
      wordBreak: 'break-all',
      overflowWrap: 'anywhere',
    }) === 2));
  check('390 overflow-wrap:anywhere min-content splits P0/P1/P2',
    ASCII_PRIORITY_TOKENS.every((token) => asciiTokenLineCount(token, anywhereCol, 8, {
      wordBreak: 'normal',
      overflowWrap: 'anywhere',
    }) === 2));
  const chatCss = fs.readFileSync(path.join(process.cwd(), 'src/chat/styles.css'), 'utf8');
  check('390 first column keeps CJK labels on one line',
    firstColumnKeepsCjkLabel(mdTableFirstColumnRule(tableCss))
      && mdTableIsScrollWrap(tableCss)
      && firstColumnKeepsCjkLabel(mdTableFirstColumnRule(chatCss))
      && mdTableIsScrollWrap(chatCss));
  const tableLayout = measurePhoneTableLineRects({
    webRule: mdTableCellRule(tableCss),
    chatRule: mdTableCellRule(chatCss),
  });
  if (tableLayout.skipped) {
    check('390 P0/P1 browser layout skipped only for a named environmental reason',
      tableLayout.reason === 'missing-agent-browser' || tableLayout.reason === 'agent-browser-wedged',
      tableLayout.reason);
    console.warn(`390 browser measurement SKIPPED (${tableLayout.reason}) — the wrap policy is still enforced by the static CSS-rule checks above`);
  } else {
    check('390 P0/P1 stay one line on /web and /chat table cells',
      tableLayout.web.p0 === 1
      && tableLayout.web.p1 === 1
      && tableLayout.chat.p0 === 1
      && tableLayout.chat.p1 === 1,
      JSON.stringify(tableLayout));
    check('390 break-all control wraps P0/P1 onto more than one line box',
      tableLayout.breakAll.p0 >= 2
      && tableLayout.breakAll.p1 >= 2
      && tableLayout.breakAll.p0Height > tableLayout.web.p0Height,
      JSON.stringify(tableLayout.breakAll));
  }
}

{
  const html = renderBlocks([
    { type: 'thinking', thinking: 'chain of thought here' },
    { type: 'text', text: 'visible answer' },
  ]);
  check('renderBlocks wraps thinking in details.thinking quote container',
    html.includes('<details class="thinking"')
      && html.includes('class="thinking__summary"')
      && html.includes('class="thinking__body"')
      && html.includes('chain of thought here')
      && html.includes('visible answer')
      && html.includes('Thinking'));
  check('renderBlocks thinking is not a bare text node',
    html.indexOf('<details class="thinking"') < html.indexOf('chain of thought here')
      && html.indexOf('chain of thought here') < html.indexOf('</details>')
      && html.includes('thinking__body'));
}

{
  const html = renderBlocks([
    { type: 'text', text: 'visible answer' },
    { type: 'thinking', thinking: 'chain of thought here' },
  ]);
  check('renderBlocks paints thinking before trailing text',
    html.indexOf('<details class="thinking"') < html.indexOf('visible answer')
      && html.indexOf('chain of thought here') < html.indexOf('visible answer'));
  const thinkingOnly = renderBlocks([{ type: 'thinking', thinking: 'only thought' }]);
  check('thinking-only stays a thinking fold',
    thinkingOnly.includes('<details class="thinking"')
      && thinkingOnly.includes('only thought')
      && thinkingOnly.indexOf('<details class="thinking"') < thinkingOnly.indexOf('only thought'));
  const stored = [
    { type: 'text', text: 'visible answer' },
    { type: 'thinking', thinking: 'chain of thought here' },
  ];
  check('thinkingFirstBlocks does not mutate storage',
    thinkingFirstBlocks(stored)[0]?.type === 'thinking'
      && stored[0]?.type === 'text'
      && stored[1]?.type === 'thinking');
  const mockThinking = 'reasoning step one\\nreasoning step two ' + 'x'.repeat(120) + '\\n推理第三段 reasoning step three';
  const mockHtml = renderBlocks([
    { type: 'thinking', thinking: mockThinking },
    { type: 'text', text: 'final answer thinking-turn-done' },
  ]);
  const bodyOpen = mockHtml.indexOf('class="thinking__body">');
  const innerStart = mockHtml.indexOf('>', bodyOpen) + 1;
  const innerEnd = mockHtml.indexOf('</div>', innerStart);
  const inner = mockHtml.slice(innerStart, innerEnd);
  const text = inner.replace(/<[^>]+>/g, '');
  check(
    'thinking body keeps real newlines from literal \\n after markdown',
    bodyOpen >= 0
      && inner === thinkingBodyInnerHtml(mockThinking)
      && (inner.match(/class="md-p"/g) ?? []).length >= 3
      && inner.includes('reasoning step one')
      && inner.includes('reasoning step two')
      && !text.includes('\\n'),
    inner.slice(0, 240),
  );
}

// Restore the environment: the shared SVG DOMParser shim only exists for
// this bundle's success paths.
if (prevDomParser === undefined) Reflect.deleteProperty(globalThis, 'DOMParser');
else Object.defineProperty(globalThis, 'DOMParser', { value: prevDomParser, configurable: true, writable: true });

console.log(`\nmarkdown.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  for (const f of failures) console.log(`  FAIL ${f}`);
  process.exit(1);
}
process.exit(0);