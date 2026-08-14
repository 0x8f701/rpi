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
  hydrateMermaid,
  diffPathLanguage,
  highlightDiffLineFragments,
} from '../src/markdown.ts';
import { safeText } from '../src/redact.ts';
// The mermaid module is aliased to scripts/__mocks__/mermaid.ts by the build
// command; its fakeDocument/tempWrappers let the leak regression observe the
// wrapper lifecycle the real mermaid 11.16.1 render reproduces.
import mermaidMock, { fakeDocument, tempWrappers } from 'mermaid';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
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

// ---- mermaid fence still renders as host div ----
{
  const html = renderFence('mermaid', 'graph TD\n  A --> B');
  check('mermaid: host div (not fence)',
    html.includes('md-mermaid-host') && !html.includes('md-fence'));
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

interface FakeHost {
  attrs: Map<string, string>;
  textContent: string;
  innerHTML: string;
  classes: Set<string>;
  getAttribute(name: string): string | null;
  setAttribute(name: string, value: string): void;
  classList: { add(cls: string): void };
}

function fakeHost(source: string): FakeHost {
  const attrs = new Map<string, string>();
  const classes = new Set<string>();
  return {
    attrs,
    textContent: source,
    innerHTML: source, // a real host's textContent IS its (escaped) innerHTML
    classes,
    getAttribute: (name: string) => attrs.get(name) ?? null,
    setAttribute: (name: string, value: string) => { attrs.set(name, value); },
    classList: { add: (cls: string) => { classes.add(cls); } },
  };
}

function fakeRoot(hosts: FakeHost[]): HTMLElement {
  return { querySelectorAll: () => hosts } as unknown as HTMLElement;
}

{
  // success path: rendered-SVG replacement fires the callback once
  const calls: string[] = [];
  const host = fakeHost('graph TD\n  A --> B');
  await hydrateMermaid(fakeRoot([host]), () => calls.push('mutated'));
  check('hydrate: svg replacement fired the callback once', calls.length === 1, `calls=${calls.length}`);
  check('hydrate: host holds the rendered svg', host.innerHTML.includes('<svg'), host.innerHTML);
  check('hydrate: rendered class added', host.classes.has('md-mermaid-host--rendered'));
  check('hydrate: host claimed done', host.getAttribute('data-mermaid') === 'done');
  check('hydrate: no error class', !host.classes.has('md-mermaid-host--error'));
}

{
  // error path: degradation mutation fires the callback once
  const calls: string[] = [];
  const host = fakeHost('BROKEN diagram');
  await hydrateMermaid(fakeRoot([host]), () => calls.push('mutated'));
  check('hydrate: error block fired the callback once', calls.length === 1, `calls=${calls.length}`);
  check('hydrate: error block rendered', host.innerHTML.includes('md-mermaid-error'), host.innerHTML);
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
  check('hub body: inline code renders', html.includes('<code class="md-code">crates/pi-cli/web/src/App.tsx</code>'), html.slice(0, 200));
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

console.log(`\nmarkdown.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  for (const f of failures) console.log(`  FAIL ${f}`);
  process.exit(1);
}
process.exit(0);