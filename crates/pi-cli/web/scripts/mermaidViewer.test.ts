#!/usr/bin/env node
// Focused node tests for src/mermaidViewer.ts — the Mermaid diagram viewer
// surface. Run via `npm run build` (esbuild + node, package.json registration).
//
// Covers, without a browser:
//   1. sanitizeMermaidSvg — the viewer/export choke point: strips
//      script/foreignObject-style elements, on* event attributes, and every
//      URL attribute that is not a local `#fragment`; keeps local refs and
//      structure; it FAILS CLOSED — parse failure and no-DOM environments
//      both degrade to '' (nothing unsanitized is shown or downloaded).
//   2. Toolbar routing — only rendered hosts (`.md-mermaid-host` WITH an svg)
//      route View/Download; error/fallback hosts and unrelated targets are
//      inert. Double-click routing only fires for the SVG of a rendered host.
//   3. No-DOM safety — openMermaidHostViewer / downloadMermaidHostSvg are
//      no-ops (never throw) in a plain-node environment.
//   4. MERMAID_HOST_TOOLBAR_HTML carries exactly the View + Download SVG row.
//   5. Viewer lifecycle (fake document) — open sanitizes and re-namespaces
//      the clone (unique `mermaid-viewer-N-` id prefix with rewritten
//      href / url(#) / ARIA / CSS refs), reopen keeps ONE stable keydown
//      handler and the ORIGINAL external restoreFocus, Escape closes, and
//      close clears the canvas before focus is restored.
//
// The DOM-dependent sanitize path is exercised through a minimal fake
// DOMParser/element shim (same convention as markdown.test.ts), extended
// with attribute/element removal and text-content support; the viewer
// lifecycle runs against a minimal fake document.
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  MERMAID_DOWNLOAD_FILE_NAME,
  MERMAID_HOST_TOOLBAR_HTML,
  MERMAID_AUTHOR_CANVAS_FILL,
  closeMermaidViewer,
  dispatchMermaidViewerActivation,
  downloadMermaidHostSvg,
  isMermaidViewerOpen,
  mermaidViewerActionFromTarget,
  openMermaidHostViewer,
  openMermaidViewer,
  sanitizeMermaidSvg,
} from '../src/mermaidViewer.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

{
  const css = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/styles.css'), 'utf8');
  const stageRule = css.match(/\.mermaid-viewer__stage\s*\{([^}]*)\}/)?.[1] ?? '';
  check('viewer stage uses touch-action:none so pointer events own pan/pinch',
    /\btouch-action\s*:\s*none\b/.test(stageRule), stageRule.trim());
}

// ---------------------------------------------------------------------
// Minimal SVG DOM shim: parse → element tree with removal support.
// ---------------------------------------------------------------------
type SvgKind = 'element' | 'text';
interface SvgNode {
  kind: SvgKind;
  name: string;
  attrs: Map<string, string>;
  children: SvgNode[];
  parent: SvgNode | null;
  text: string;
}
const parseAttrs = (raw: string): Map<string, string> => {
  const attrs = new Map<string, string>();
  const re = /([^\s=]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'))?/g;
  for (let match = re.exec(raw); match; match = re.exec(raw)) {
    attrs.set(match[1], match[2] ?? match[3] ?? '');
  }
  return attrs;
};
const parseSvg = (input: string): SvgNode => {
  const root: SvgNode = { kind: 'element', name: '#root', attrs: new Map(), children: [], parent: null, text: '' };
  const stack: SvgNode[] = [root];
  let i = 0;
  while (i < input.length) {
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
        attrs: parseAttrs(sp < 0 ? '' : open.slice(sp)),
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
const serialize = (node: SvgNode): string => {
  if (node.kind === 'text') return node.text;
  const attr = [...node.attrs.entries()].map(([k, v]) => ` ${k}="${v}"`).join('');
  const inner = node.children.map(serialize).join('');
  if (node.name === '#root') return inner;
  return `<${node.name}${attr}>${inner}</${node.name}>`;
};
const wrapElement = (node: SvgNode): Element => {
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
    get nodeName() { return node.name; },
    get outerHTML() { return serialize(node); },
    attributes: {
      get length() { return node.attrs.size; },
      item(index: number) { return attrList()[index] ?? null; },
    },
    getAttribute(name: string) { return node.attrs.get(name) ?? null; },
    setAttribute(name: string, value: string) { node.attrs.set(name, value); },
    removeAttribute(name: string) { node.attrs.delete(name); },
    remove() {
      const parent = node.parent;
      if (!parent) return;
      const index = parent.children.indexOf(node);
      if (index >= 0) parent.children.splice(index, 1);
      node.parent = null;
    },
    get textContent() {
      return node.children.filter((child) => child.kind === 'text').map((child) => child.text).join('');
    },
    set textContent(value: string) {
      node.children = [{ kind: 'text', name: '#text', attrs: new Map(), children: [], parent: node, text: String(value) }];
    },
    querySelectorAll(selector: string) {
      const all = descendants(node);
      const matched = selector === '*' ? all : all.filter((child) => child.name === selector);
      return matched.map(wrapElement);
    },
    querySelector(selector: string) {
      return element.querySelectorAll(selector)[0] ?? null;
    },
  };
  return element as unknown as Element;
};
class SvgDomParser {
  parseFromString(source: string, _type: string) {
    // '&nbsp;' is the parse-failure sentinel (mirrors markdown.test.ts).
    if (source.includes('&nbsp;')) {
      throw new Error('parsererror');
    }
    return { documentElement: wrapElement(parseSvg(source)) };
  }
}

const prevParser = Reflect.get(globalThis, 'DOMParser');
Object.defineProperty(globalThis, 'DOMParser', { value: SvgDomParser, configurable: true, writable: true });

// ---------------------------------------------------------------------
// 1. sanitizeMermaidSvg
// ---------------------------------------------------------------------
{
  // Hazard strip: script/foreignObject elements and on* attributes.
  const dirty = '<svg id="s1" width="10" onload="evil()" onclick="evil()"><script>evil()</script><foreignObject><p>x</p></foreignObject><rect width="1" height="1"/><iframe src="https://x"></iframe></svg>';
  const clean = sanitizeMermaidSvg(dirty);
  check('sanitize: script element removed', !clean.includes('<script'), clean);
  check('sanitize: foreignObject element removed', !clean.includes('foreignobject') && !clean.includes('foreignObject'), clean);
  check('sanitize: iframe element removed', !clean.includes('<iframe'), clean);
  check('sanitize: onload attribute removed', !clean.includes('onload'), clean);
  check('sanitize: onclick attribute removed', !clean.includes('onclick'), clean);
  check('sanitize: safe content preserved', clean.includes('<rect') && clean.includes('id="s1"'), clean);
}
{
  // URL attributes: only local `#fragment` refs survive.
  const dirty = '<svg><defs><marker id="a"/></defs><use href="https://evil.example/x.svg#e"/><a xlink:href="javascript:alert(1)"><rect/></a><image href="data:image/png;base64,AAAA"/><path href="#a" xlink:href="#a"/><use href="#a"/></svg>';
  const clean = sanitizeMermaidSvg(dirty);
  check('sanitize: external https href removed', !clean.includes('evil.example'), clean);
  check('sanitize: javascript: href removed', !clean.includes('javascript:'), clean);
  check('sanitize: data: href removed', !clean.includes('data:image'), clean);
  check('sanitize: local # href preserved', (clean.match(/href="#a"/g) ?? []).length >= 2, clean);
}
{
  // CSS hazards in <style> content and style attributes: @import and every
  // non-local url() removed; local url(#id) preserved for the viewer clone.
  const dirty = '<svg><defs><marker id="m"/></defs><style>@import url("https://evil.example/i.css");@import "https://evil.example/b.css";.a{fill:url(#m);background:url(https://evil.example/x.png)}</style><rect style="fill:url(#m);background:url(javascript:alert(1))" width="1" height="1"/></svg>';
  const clean = sanitizeMermaidSvg(dirty);
  check('sanitize: style element @import removed', !/@import/i.test(clean), clean);
  check('sanitize: style element external url removed', !clean.includes('evil.example'), clean);
  check('sanitize: style element local url preserved', /fill:\s*url\(#m\)/.test(clean), clean);
  check('sanitize: style attribute external url removed', !clean.includes('javascript:'), clean);
  check('sanitize: style attribute local url preserved', clean.includes('fill:url(#m)'), clean);
}
{
  // Parse failure degrades to '' — sanitize never fails open in a browser.
  check('sanitize: parse failure yields empty string', sanitizeMermaidSvg('<svg><text>a&nbsp;b</text></svg>') === '');
  // Non-SVG / non-string input yields '' (nothing to show or download).
  check('sanitize: non-svg string yields empty', sanitizeMermaidSvg('<div>hi</div>') === '');
  check('sanitize: empty string yields empty string', sanitizeMermaidSvg('') === '');
}
{
  // No DOMParser (plain node without the shim): sanitize FAILS CLOSED —
  // nothing unsanitized is ever shown or downloaded (viewer/export contract).
  const hadParser = prevParser !== undefined;
  if (prevParser === undefined) Reflect.deleteProperty(globalThis, 'DOMParser');
  else Object.defineProperty(globalThis, 'DOMParser', { value: prevParser, configurable: true, writable: true });
  check('sanitize: no-DOM environment fails closed (empty string)',
    sanitizeMermaidSvg('<svg><rect/></svg>') === '');
  if (hadParser) Object.defineProperty(globalThis, 'DOMParser', { value: SvgDomParser, configurable: true, writable: true });
}
// ---------------------------------------------------------------------
// 2. Toolbar routing + double-click shortcut
// ---------------------------------------------------------------------
interface FakeNode {
  name: string;
  classes: string[];
  attrs: Record<string, string>;
  children: FakeNode[];
  parent: FakeNode | null;
}
function fakeNode(options: Partial<FakeNode> & { name: string }): FakeNode {
  const node: FakeNode = {
    name: options.name,
    classes: options.classes ?? [],
    attrs: options.attrs ?? {},
    children: [],
    parent: null,
  };
  for (const child of options.children ?? []) {
    child.parent = node;
    node.children.push(child);
  }
  return node;
}
const fakeWrappers = new WeakMap<FakeNode, Element>();
function wrapFake(node: FakeNode): Element {
  const cached = fakeWrappers.get(node);
  if (cached) return cached;
  const element = {
    closest(selector: string): unknown {
      let current: FakeNode | null = node;
      while (current) {
        if (
          (selector === 'svg' && current.name === 'svg')
          || (selector === '.md-mermaid-host' && current.classes.includes('md-mermaid-host'))
          || (selector === '.md-mermaid-host--rendered' && current.classes.includes('md-mermaid-host--rendered'))
          || (selector === '[data-mermaid-action]' && current.attrs['data-mermaid-action'] !== undefined)
        ) return wrapFake(current);
        current = current.parent;
      }
      return null;
    },
    getAttribute(name: string): string | null { return node.attrs[name] ?? null; },
    querySelector(selector: string): unknown {
      if (selector !== 'svg') return null;
      const hit = node.children.find((child) => child.name === 'svg');
      return hit ? wrapFake(hit) : null;
    },
  } as unknown as Element;
  fakeWrappers.set(node, element);
  return element;
}

{
  // A rendered host: toolbar button → host → svg.
  const svg = fakeNode({ name: 'svg' });
  const toolbar = fakeNode({
    name: 'div',
    classes: ['md-mermaid-toolbar'],
    children: [
      fakeNode({ name: 'button', attrs: { 'data-mermaid-action': 'view' } }),
      fakeNode({ name: 'button', attrs: { 'data-mermaid-action': 'download' } }),
    ],
  });
  const host = fakeNode({ name: 'div', classes: ['md-mermaid-host', 'md-mermaid-host--rendered'], children: [toolbar, svg] });

  const viewButton = toolbar.children[0];
  const routedView = mermaidViewerActionFromTarget(wrapFake(viewButton));
  check('toolbar: View button routes to the host', routedView !== null && routedView.action === 'view' && routedView.host === wrapFake(host), JSON.stringify(routedView));
  const routedDownload = mermaidViewerActionFromTarget(wrapFake(toolbar.children[1]));
  check('toolbar: Download button routes to the host', routedDownload !== null && routedDownload.action === 'download', JSON.stringify(routedDownload));

  // Unrelated targets never route.
  check('toolbar: click outside a toolbar routes nothing', mermaidViewerActionFromTarget(wrapFake(svg)) === null);
  check('toolbar: text-free target routes nothing', mermaidViewerActionFromTarget(null) === null);
  const strayUnknown = fakeNode({ name: 'button', attrs: { 'data-mermaid-action': 'other' } });
  fakeNode({ name: 'div', classes: ['md-mermaid-host'], children: [strayUnknown, fakeNode({ name: 'svg' })] });
  check('toolbar: unknown action value routes nothing',
    mermaidViewerActionFromTarget(wrapFake(strayUnknown)) === null);

  // Double-click: only the SVG of a RENDERED host is the View shortcut.
  check('dblclick: rendered-host svg opens the viewer',
    dispatchMermaidViewerActivation(wrapFake(svg)) !== null);
  const plainHost = fakeNode({ name: 'div', classes: ['md-mermaid-host'], children: [fakeNode({ name: 'svg' })] });
  check('dblclick: non-rendered host svg is inert',
    dispatchMermaidViewerActivation(wrapFake(plainHost.children[0])) === null);
  check('dblclick: non-svg target is inert',
    dispatchMermaidViewerActivation(wrapFake(toolbar)) === null);
}
{
  // An ERROR host (fallback block, no svg) must never route — the toolbar
  // does not exist there and a stray button stays inert.
  const stray = fakeNode({ name: 'button', attrs: { 'data-mermaid-action': 'view' } });
  const errorHost = fakeNode({ name: 'div', classes: ['md-mermaid-host', 'md-mermaid-host--error'], children: [stray] });
  check('toolbar: error host without svg routes nothing',
    mermaidViewerActionFromTarget(wrapFake(stray)) === null);
  check('dblclick: error host without svg is inert',
    dispatchMermaidViewerActivation(wrapFake(errorHost)) === null);
}

// ---------------------------------------------------------------------
// 3. No-DOM safety (plain node: no document) — never throws, never opens.
// ---------------------------------------------------------------------
{
  const svg = fakeNode({ name: 'svg' });
  const toolbar = fakeNode({
    name: 'div',
    children: [fakeNode({ name: 'button', attrs: { 'data-mermaid-action': 'view' } })],
  });
  const host = fakeNode({ name: 'div', classes: ['md-mermaid-host'], children: [toolbar, svg] });
  let threw = false;
  try {
    openMermaidHostViewer(wrapFake(host));
    downloadMermaidHostSvg(wrapFake(host));
  } catch {
    threw = true;
  }
  check('no-DOM: host viewer/download calls are safe no-ops', !threw);
  check('no-DOM: viewer never opened', isMermaidViewerOpen() === false);
}

// ---------------------------------------------------------------------
// 4. Toolbar markup contract
// ---------------------------------------------------------------------
{
  check('toolbar: View button present',
    MERMAID_HOST_TOOLBAR_HTML.includes('data-mermaid-action="view"') && MERMAID_HOST_TOOLBAR_HTML.includes('>View<'),
    MERMAID_HOST_TOOLBAR_HTML);
  check('toolbar: Download SVG button present',
    MERMAID_HOST_TOOLBAR_HTML.includes('data-mermaid-action="download"') && MERMAID_HOST_TOOLBAR_HTML.includes('>Download SVG<'),
    MERMAID_HOST_TOOLBAR_HTML);
  check('toolbar: single always-visible row',
    MERMAID_HOST_TOOLBAR_HTML.startsWith('<div class="md-mermaid-toolbar">') && MERMAID_HOST_TOOLBAR_HTML.endsWith('</div>'),
    MERMAID_HOST_TOOLBAR_HTML);
  check('download: fixed vector file name', MERMAID_DOWNLOAD_FILE_NAME === 'mermaid-diagram.svg');
}

// ---------------------------------------------------------------------
// 5. Viewer lifecycle (fake document): sanitize + clone namespacing, one
//    stable keydown handler across open/open/Escape, original external
//    restoreFocus, clear-on-close.
// ---------------------------------------------------------------------

// `document.activeElement instanceof Element` (openMermaidViewer) needs a
// real global Element constructor in plain node.
const prevViewerElement = Reflect.get(globalThis, 'Element');
if (prevViewerElement === undefined) {
  Object.defineProperty(globalThis, 'Element', { value: class MermaidViewerTestElement {}, configurable: true, writable: true });
}

const VIEWER_SVG_WITH_IDS =
  '<svg viewBox="0 0 100 100">' +
  '<defs><marker id="m" refX="0"/></defs>' +
  '<style>.a{fill:url(#m);stroke:#abc}#t1{font-weight:bold}</style>' +
  '<title id="t1">Node</title><desc id="t2">Detail</desc>' +
  '<path marker-end="url(#m)" href="#m" aria-labelledby="t1 t2"/></svg>';

class FakeViewerElement extends Element {
  tagName: string;
  attrs = new Map<string, string>();
  children: FakeViewerElement[] = [];
  parent: FakeViewerElement | null = null;
  hidden = false;
  tabIndex = 0;
  disabled = false;
  text = '';
  style: Record<string, string> = {};

  constructor(tagName: string) {
    super();
    this.tagName = tagName.toLowerCase();
  }

  contains(node: Element | null): boolean {
    return node === this || this.children.some((child) => child.contains(node));
  }

  blur(): void {
    if (currentFakeDoc?.activeElement === this) currentFakeDoc.activeElement = currentFakeDoc.body;
  }

  get className(): string { return this.attrs.get('class') ?? ''; }
  set className(value: string) { this.attrs.set('class', value); }

  get innerHTML(): string {
    const serialize = (el: FakeViewerElement): string => {
      const attrText = [...el.attrs.entries()].map(([k, v]) => ` ${k}="${v}"`).join('');
      return `<${el.tagName}${attrText}>${el.text}${el.children.map(serialize).join('')}</${el.tagName}>`;
    };
    return this.children.map(serialize).join('');
  }
  set innerHTML(value: string) {
    this.children = [];
    const stack: FakeViewerElement[] = [this];
    const tagRe = /<(\/?)([\w-]+)((?:\s+[\w:-]+(?:="[^"]*")?)*)\s*(\/?)>/g;
    let cursor = 0;
    for (let match = tagRe.exec(value); match; match = tagRe.exec(value)) {
      const text = value.slice(cursor, match.index).trim();
      if (text !== '') stack[stack.length - 1].text += text;
      cursor = tagRe.lastIndex;
      if (match[1] === '/') { if (stack.length > 1) stack.pop(); continue; }
      const child = new FakeViewerElement(match[2]);
      child.parent = stack[stack.length - 1];
      stack[stack.length - 1].children.push(child);
      for (const attr of match[3].matchAll(/([\w:-]+)(?:="([^"]*)")?/g)) {
        if (attr[1]) child.attrs.set(attr[1], attr[2] ?? '');
      }
      if (!match[4]) stack.push(child);
    }
    const tail = value.slice(cursor).trim();
    if (tail !== '') stack[stack.length - 1].text += tail;
  }

  get firstElementChild(): FakeViewerElement | null { return this.children[0] ?? null; }
  get textContent(): string { return this.text; }
  set textContent(value: string) { this.text = String(value); }

  getAttribute(name: string): string | null { return this.attrs.get(name) ?? null; }
  setAttribute(name: string, value: string): void { this.attrs.set(name, value); }
  removeAttribute(name: string): void { this.attrs.delete(name); }
  appendChild(child: FakeViewerElement): FakeViewerElement { child.parent = this; this.children.push(child); return child; }

  private walk(out: FakeViewerElement[]): void {
    for (const child of this.children) { out.push(child); child.walk(out); }
  }
  matches(selector: string): boolean {
    const tag = /^[a-zA-Z][\w-]*/.exec(selector)?.[0] ?? '';
    const rest = tag ? selector.slice(tag.length) : selector;
    if (tag !== '' && this.tagName !== tag.toLowerCase()) return false;
    for (const attrMatch of rest.matchAll(/\[([\w:-]+)(?:="([^"]*)")?\]/g)) {
      const value = this.attrs.get(attrMatch[1]);
      if (attrMatch[2] === undefined ? value === undefined : value !== attrMatch[2]) return false;
    }
    for (const classMatch of rest.matchAll(/\.([\w-]+)/g)) {
      if (!(this.attrs.get('class') ?? '').split(/\s+/).includes(classMatch[1])) return false;
    }
    return true;
  }
  querySelector<T extends Element>(selector: string): T | null {
    return this.querySelectorAll<FakeViewerElement>(selector)[0] as T ?? null;
  }
  querySelectorAll<T extends Element>(selector: string): T[] {
    const all: FakeViewerElement[] = [];
    this.walk(all);
    return all.filter((el) => el.matches(selector)) as unknown as T[];
  }

  addEventListener(type: string, handler: (event: unknown) => void): void {
    this.viewerListeners.set(type, [...(this.viewerListeners.get(type) ?? []), handler]);
  }
  emit(type: string, event: unknown): void {
    for (const handler of this.viewerListeners.get(type) ?? []) handler(event);
  }
  closest(selector: string): FakeViewerElement | null {
    let node: FakeViewerElement | null = this;
    while (node) {
      if (node.matches(selector)) return node;
      node = node.parent;
    }
    return null;
  }
  getBoundingClientRect(): { width: number; height: number; left: number; top: number; right: number; bottom: number } {
    return { width: 1200, height: 800, left: 0, top: 0, right: 1200, bottom: 800 };
  }
  pointerCaptures = new Set<number>();
  setPointerCapture(pointerId: number): void { this.pointerCaptures.add(pointerId); }
  hasPointerCapture(pointerId: number): boolean { return this.pointerCaptures.has(pointerId); }
  releasePointerCapture(pointerId: number): void { this.pointerCaptures.delete(pointerId); }
  focus(): void { if (currentFakeDoc) currentFakeDoc.activeElement = this; }

  private viewerListeners = new Map<string, ((event: unknown) => void)[]>();
}

interface FakeViewerDocument {
  body: FakeViewerElement;
  activeElement: Element | null;
  keydownHandlers: ((event: KeyboardEvent) => void)[];
}

let currentFakeDoc: FakeViewerDocument | null = null;
let fakeDocumentGlobal: Record<string, unknown> | null = null;

const makeFakeViewerDocument = (): FakeViewerDocument => {
  const doc: FakeViewerDocument = { body: new FakeViewerElement('body'), activeElement: null, keydownHandlers: [] };
  currentFakeDoc = doc;
  fakeDocumentGlobal = {
    body: doc.body,
    get activeElement() { return doc.activeElement; },
    set activeElement(value: Element | null) { doc.activeElement = value; },
    createElement: (tag: string) => new FakeViewerElement(tag),
    addEventListener: (type: string, handler: (event: KeyboardEvent) => void) => {
      if (type === 'keydown') doc.keydownHandlers.push(handler);
    },
    removeEventListener: (_type: string, _handler: (event: KeyboardEvent) => void) => {},
  };
  Object.defineProperty(globalThis, 'document', { value: fakeDocumentGlobal, configurable: true, writable: true });
  return doc;
};

{
  const prevDocument = Reflect.get(globalThis, 'document');
  const prevParserForViewer = Reflect.get(globalThis, 'DOMParser');
  try {
    // The no-DOM sanitize test above deletes globalThis.DOMParser (plain
    // node has none, so its restore is a no-op); the viewer pipeline parses,
    // so re-arm the SVG shim here and restore it on exit.
    Object.defineProperty(globalThis, 'DOMParser', { value: SvgDomParser, configurable: true, writable: true });
    const doc = makeFakeViewerDocument();
    const external = new FakeViewerElement('button');
    doc.activeElement = external;

    // First open: sanitized clone inserted, ids namespaced + unique, refs rewritten.
    openMermaidViewer(VIEWER_SVG_WITH_IDS);
    check('viewer: opens', isMermaidViewerOpen());
    const viewerRoot = doc.body.children.find((child) => child.attrs.get('class') === 'mermaid-viewer') ?? null;
    check('viewer: dialog appended and visible', viewerRoot !== null && viewerRoot.hidden === false);
    const canvas = viewerRoot?.querySelector<FakeViewerElement>('.mermaid-viewer__canvas') ?? null;
    check('viewer: canvas holds exactly one svg clone', canvas !== null && canvas.children.length === 1 && canvas.children[0].tagName === 'svg');

    const collectIds = (el: FakeViewerElement, out: string[]): void => {
      const id = el.attrs.get('id');
      if (id !== undefined) out.push(id);
      for (const child of el.children) collectIds(child, out);
    };
    const ids: string[] = [];
    if (canvas) collectIds(canvas, ids);
    check('viewer: clone ids all carry the viewer prefix',
      ids.length > 0 && ids.every((id) => id.startsWith('mermaid-viewer-')), ids.join(','));
    check('viewer: no duplicate clone ids', new Set(ids).size === ids.length, ids.join(','));
    const markup = canvas !== null ? canvas.innerHTML : '';
    check('viewer: url(#id) refs rewritten', markup.includes('url(#mermaid-viewer-'), markup);
    check('viewer: href fragments rewritten', markup.includes('href="#mermaid-viewer-'), markup);
    check('viewer: aria idref list rewritten', markup.includes('aria-labelledby="mermaid-viewer-1-t1 mermaid-viewer-1-t2"'), markup);
    check('viewer: css style url ref rewritten', /fill:\s*url\(#mermaid-viewer-1-m\)/.test(markup), markup);
    check('viewer: css id selector rewritten', markup.includes('#mermaid-viewer-1-t1{'), markup);
    check('viewer: hex-color token untouched', /stroke:\s*#abc/.test(markup), markup);
    const closeButton = viewerRoot?.querySelector<FakeViewerElement>('button[data-viewer-action="close"]') ?? null;
    check('viewer: close button focused on open', closeButton !== null && doc.activeElement === closeButton);
    check('viewer: exactly one keydown handler after open', doc.keydownHandlers.length === 1, String(doc.keydownHandlers.length));

    // Reopen while open: no second handler, fresh namespaced clone, original
    // external restoreFocus preserved (never re-captured from the viewer).
    openMermaidViewer(VIEWER_SVG_WITH_IDS);
    check('viewer: reopen keeps exactly one keydown handler', doc.keydownHandlers.length === 1, String(doc.keydownHandlers.length));
    check('viewer: reopen keeps the viewer open', isMermaidViewerOpen());
    const reopenedIds: string[] = [];
    if (canvas) collectIds(canvas, reopenedIds);
    check('viewer: reopen re-namespaces clone ids',
      reopenedIds.length > 0 && reopenedIds.every((id) => id.startsWith('mermaid-viewer-2-')), reopenedIds.join(','));
    check('viewer: reopen keeps focus inside the viewer', doc.activeElement === closeButton);

    // Escape (via the one stable handler) closes: canvas cleared, the
    // ORIGINAL external element gets focus back.
    doc.keydownHandlers[0]?.({ key: 'Escape', preventDefault() {}, shiftKey: false } as unknown as KeyboardEvent);
    check('viewer: Escape closes the viewer', isMermaidViewerOpen() === false);
    check('viewer: close clears the canvas', canvas !== null && canvas.innerHTML === '');
    check('viewer: close hides the dialog', viewerRoot !== null && viewerRoot.hidden === true);
    check('viewer: Escape restores ORIGINAL external focus', doc.activeElement === external);
    check('viewer: stable handler stays registered after close', doc.keydownHandlers.length === 1);

    // Full open/close cycle after Escape: singleton handler still exactly one.
    doc.activeElement = external;
    openMermaidViewer(VIEWER_SVG_WITH_IDS);
    check('viewer: reopens after close', isMermaidViewerOpen());
    check('viewer: still one keydown handler after full cycle', doc.keydownHandlers.length === 1, String(doc.keydownHandlers.length));
    closeMermaidViewer();
    check('viewer: explicit close clears canvas and closes',
      isMermaidViewerOpen() === false && canvas !== null && canvas.innerHTML === '');
    check('viewer: explicit close restores external focus', doc.activeElement === external);

    openMermaidViewer(VIEWER_SVG_WITH_IDS, null, 'author');
    check('viewer: author palette pads a white canvas',
      canvas !== null && canvas.style.background === MERMAID_AUTHOR_CANVAS_FILL,
      canvas ? canvas.style.background : 'missing-canvas');
    closeMermaidViewer();
    check('viewer: close clears the author canvas fill',
      canvas !== null && canvas.style.background === '');

    const stage = viewerRoot?.querySelector<FakeViewerElement>('.mermaid-viewer__stage') ?? null;
    check('viewer: stage is present', stage !== null);
    openMermaidViewer(VIEWER_SVG_WITH_IDS);
    const pointerDown = { pointerId: 11, clientX: 40, clientY: 50 };
    stage?.emit('pointerdown', pointerDown);
    check('viewer: pointerdown captures the pointer on the stage',
      stage !== null && stage.pointerCaptures.has(11), JSON.stringify([...stage?.pointerCaptures ?? []]));
    stage?.emit('pointerup', { pointerId: 11, clientX: 42, clientY: 52 });
    check('viewer: pointerup releases stage pointer capture',
      stage !== null && !stage.pointerCaptures.has(11), JSON.stringify([...stage?.pointerCaptures ?? []]));
    const closeAfterUp = viewerRoot?.querySelector<FakeViewerElement>('button[data-viewer-action="close"]') ?? null;
    viewerRoot?.emit('click', { target: closeAfterUp, preventDefault() {} });
    check('viewer: Close click after pointerup closes (click synthesis)', isMermaidViewerOpen() === false);
    check('viewer: Close click after pointerup leaves no capture',
      stage !== null && stage.pointerCaptures.size === 0, JSON.stringify([...stage?.pointerCaptures ?? []]));

    openMermaidViewer(VIEWER_SVG_WITH_IDS);
    stage?.emit('pointerdown', { pointerId: 12, clientX: 10, clientY: 20 });
    check('viewer: capture is held before close-without-up',
      stage !== null && stage.pointerCaptures.has(12));
    closeMermaidViewer();
    check('viewer: close releases leftover pointer capture',
      isMermaidViewerOpen() === false && stage !== null && stage.pointerCaptures.size === 0,
      JSON.stringify([...stage?.pointerCaptures ?? []]));

    openMermaidViewer(VIEWER_SVG_WITH_IDS);
    stage?.emit('pointerdown', { pointerId: 13, clientX: 8, clientY: 9 });
    stage?.emit('pointercancel', { pointerId: 13, clientX: 8, clientY: 9 });
    check('viewer: pointercancel releases stage pointer capture',
      stage !== null && !stage.pointerCaptures.has(13), JSON.stringify([...stage?.pointerCaptures ?? []]));
    closeMermaidViewer();

    let continued = 0;
    external.addEventListener('click', () => { continued += 1; });
    external.emit('click', { preventDefault() {} });
    check('viewer: after close, other elements still receive click', continued === 1, String(continued));
  } finally {
    currentFakeDoc = null;
    fakeDocumentGlobal = null;
    Object.defineProperty(globalThis, 'DOMParser', { value: prevParserForViewer, configurable: true, writable: true });
    Object.defineProperty(globalThis, 'document', { value: prevDocument, configurable: true, writable: true });
    if (prevViewerElement === undefined) {
      Object.defineProperty(globalThis, 'Element', { value: undefined, configurable: true, writable: true });
    }
  }
}

Object.defineProperty(globalThis, 'DOMParser', {
  configurable: true,
  writable: true,
  value: prevParser,
});

if (failures.length > 0) {
  for (const failure of failures) console.error(`not ok - ${failure}`);
  process.exit(1);
}
console.log(`mermaid viewer tests passed (${ran} checks)`);