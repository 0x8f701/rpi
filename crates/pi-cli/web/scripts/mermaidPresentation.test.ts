import fs from 'node:fs';
import path from 'node:path';
import mermaidMock, { initState } from 'mermaid';
import { hydrateMermaid, mermaidSvgCacheTest } from '../src/markdown.ts';

// ---------------------------------------------------------------------
// Minimal SVG DOMParser shim (mermaidViewer.test.ts convention): the
// sanitize/instantiate pipeline parses mermaid.render output through
// DOMParser and FAILS CLOSED when there is none, so the intended success
// path needs this shim under plain node. No innerHTML parsing fallback
// exists in production and none is shimmed here. '&nbsp;' is the parse-
// failure sentinel (an undefined entity in XML): parseFromString returns
// a parsererror document, mirroring the browser.
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

let failures = 0;
function check(name: string, ok: boolean, detail = ''): void {
  if (ok) {
    console.log(`ok - ${name}`);
    return;
  }
  failures += 1;
  console.error(`not ok - ${name}${detail ? `: ${detail}` : ''}`);
}

interface FakeHost {
  textContent: string;
  innerHTML: string;
  classes: Set<string>;
  isConnected: boolean;
  clientWidth: number;
  attributes: Record<string, string>;
  querySelector(selector: string): FakeSvg | null;
  classList: { add(name: string): void };
  getAttribute(name: string): string | null;
  setAttribute(name: string, value: string): void;
  removeAttribute(name: string): void;
}
interface FakeSvg {
  getAttribute(name: string): string | null;
  removeAttribute(name: string): void;
  setAttribute(name: string, value: string): void;
  style: { minWidth: string; background: string };
}

function fakeRoot(hosts: FakeHost[]): HTMLElement {
  return { isConnected: true, querySelectorAll: () => hosts } as unknown as HTMLElement;
}

function fakeHost(source: string): FakeHost {
  const host: FakeHost = {
    textContent: source,
    innerHTML: source,
    classes: new Set(),
    isConnected: true,
    clientWidth: 320,
    attributes: {},
    querySelector(selector: string): FakeSvg | null {
      if (selector !== 'svg' || !this.innerHTML.includes('<svg')) return null;
      // Attribute reads/writes target the LIVE svg open tag: production
      // mutates attributes between calls (style removal, width recovery),
      // so a snapshot taken at querySelector time would go stale.
      const openTag = (): string => this.innerHTML.match(/<svg\b[^>]*>/)?.[0] ?? '';
      return {
        style: {
          merge(name: string, value: string): void {
            const tag = openTag();
            if (tag === '') return;
            const attribute = /\sstyle="([^"]*)"/;
            const current = tag.match(attribute)?.[1] ?? '';
            const parts = current.split(';').map((part) => part.trim()).filter((part) => part !== '' && !part.startsWith(`${name}:`));
            parts.push(`${name}: ${value}`);
            const style = `${parts.join('; ')};`;
            const next = attribute.test(tag)
              ? tag.replace(attribute, ` style="${style}"`)
              : tag.replace('<svg', `<svg style="${style}"`);
            host.innerHTML = host.innerHTML.replace(tag, next);
          },
          get minWidth(): string {
            return host.innerHTML.match(/\sstyle="[^"]*min-width:\s*([^;"]+)/)?.[1] ?? '';
          },
          set minWidth(value: string) {
            this.merge('min-width', value);
          },
          get background(): string {
            return host.innerHTML.match(/\sstyle="[^"]*background:\s*([^;"]+)/)?.[1] ?? '';
          },
          set background(value: string) {
            this.merge('background', value);
          },
        },
        getAttribute(name: string): string | null {
          const match = openTag().match(new RegExp(`\\s${name}="([^"]*)"`));
          return match?.[1] ?? null;
        },
        removeAttribute(name: string): void {
          const tag = openTag();
          if (tag === '') return;
          host.innerHTML = host.innerHTML.replace(tag, tag.replace(new RegExp(`\\s${name}="[^"]*"`), ''));
        },
        setAttribute(name: string, value: string): void {
          const attribute = new RegExp(`\\s${name}="[^"]*"`);
          const tag = openTag();
          if (tag === '') return;
          host.innerHTML = attribute.test(tag)
            ? host.innerHTML.replace(tag, tag.replace(attribute, ` ${name}="${value}"`))
            : host.innerHTML.replace('<svg', `<svg ${name}="${value}"`);
        },
      };
    },
    classList: { add(name: string): void { host.classes.add(name); } },
    getAttribute(name: string): string | null { return this.attributes[name] ?? null; },
    setAttribute(name: string, value: string): void { this.attributes[name] = value; },
    removeAttribute(name: string): void { delete this.attributes[name]; },
  };
  return host;
}

const css = fs.readFileSync(path.join(process.cwd(), 'src/styles.css'), 'utf8');
const hostRule = css.match(/\.md-mermaid-host\s*\{([^}]*)\}/)?.[1] ?? '';
const viewportRule = css.match(/\.md-mermaid-viewport\s*\{([^}]*)\}/)?.[1] ?? '';
const svgRule = css.match(/\.md-mermaid-viewport svg\s*\{([^}]*)\}/)?.[1] ?? '';
const toolbarRule = css.match(/\.md-mermaid-toolbar\s*\{([^}]*)\}/)?.[1] ?? '';
const viewerRule = css.match(/\.mermaid-viewer\s*\{([^}]*)\}/)?.[1] ?? '';
const stageRule = css.match(/\.mermaid-viewer__stage\s*\{([^}]*)\}/)?.[1] ?? '';
check('mermaid host has no light edge border', !/\bborder\s*:/.test(hostRule), hostRule.trim());
check('mermaid host has no contrasting panel fill', /\bbackground\s*:\s*transparent\b/.test(hostRule), hostRule.trim());
check('mermaid host stacks toolbar above the diagram (separate rows)', /\bflex-direction\s*:\s*column\b/.test(hostRule), hostRule.trim());
check('mermaid host never flex-centers', !/\bjustify-content\s*:\s*center\b/.test(hostRule), hostRule.trim());
check('mermaid viewport is the horizontal scroller', /\boverflow-x\s*:\s*auto\b/.test(viewportRule), viewportRule.trim());
check('mermaid svg preserves intrinsic width for overflow', /\bmax-width\s*:\s*100%/.test(svgRule), svgRule.trim());
check('mermaid svg centers only when it fits', /\bmargin-inline\s*:\s*auto\b/.test(svgRule), svgRule.trim());
check('host toolbar row is always visible (non-scrolling flex row)',
  /\bdisplay\s*:\s*flex\b/.test(toolbarRule) && /\bflex\s*:\s*none\b/.test(toolbarRule), toolbarRule.trim());
check('viewer is a full-screen overlay', /\bposition\s*:\s*fixed\b/.test(viewerRule) && /\binset\s*:\s*0\b/.test(viewerRule) && /\bz-index\s*:\s*70\b/.test(viewerRule), viewerRule.trim());
check('viewer hides fully when closed', /\.mermaid-viewer\[hidden\]\s*\{[^}]*display:\s*none/.test(css));
check('viewer stage owns pan/zoom gestures (no native scroll)', /\btouch-action\s*:\s*none\b/.test(stageRule), stageRule.trim());

const originalRender = mermaidMock.render;
mermaidMock.render = async () => ({
  svg: '<svg class="mock-svg" width="100%" style="max-width: 640px" viewBox="0 0 640 120"></svg>',
});
try {
  const host = fakeHost('flowchart LR\nA --> B');
  await hydrateMermaid(fakeRoot([host]));
  const initConfig = initState.config as {
    theme?: string;
    securityLevel?: string;
    htmlLabels?: boolean;
    themeVariables?: Record<string, string | boolean>;
  } | undefined;
  check('initMermaid keeps mermaid dark theme', initConfig?.theme === 'dark', JSON.stringify(initConfig));
  check('initMermaid keeps securityLevel strict', initConfig?.securityLevel === 'strict', JSON.stringify(initConfig));
  check('initMermaid disables html labels', initConfig?.htmlLabels === false, JSON.stringify(initConfig));
  check(
    'initMermaid overrides mermaid dark #ccc nodeBorder',
    initConfig?.themeVariables?.nodeBorder === '#3a4350'
      && initConfig?.themeVariables?.border1 === '#3a4350'
      && initConfig?.themeVariables?.clusterBorder === '#3a4350',
    JSON.stringify(initConfig?.themeVariables),
  );
  check(
    'initMermaid cylinder/node fill is panel with fg text',
    initConfig?.themeVariables?.primaryColor === '#151820'
      && initConfig?.themeVariables?.primaryTextColor === '#e8ecf4'
      && initConfig?.themeVariables?.nodeBkg === '#151820'
      && initConfig?.themeVariables?.useGradient === false,
    JSON.stringify(initConfig?.themeVariables),
  );
  check(
    'initMermaid never reintroduces hardcoded #ccc stroke',
    Object.values(initConfig?.themeVariables ?? {}).every((value) => value !== '#ccc'),
    JSON.stringify(initConfig?.themeVariables),
  );
  check('hydration replaces Mermaid max-width with intrinsic minimum', host.querySelector('svg')?.getAttribute('style') === 'min-width: 640px;', host.innerHTML);
  check('hydration preserves intrinsic SVG width', host.querySelector('svg')?.getAttribute('width') === '640', host.innerHTML);
  check('hydration preserves intrinsic SVG height', host.querySelector('svg')?.getAttribute('height') === '120', host.innerHTML);
  check('hydration preserves SVG viewBox', host.querySelector('svg')?.getAttribute('viewBox') === '0 0 640 120', host.innerHTML);
  check('hydration marks wide SVG for scrolling', host.querySelector('svg')?.getAttribute('data-mermaid-wide') === 'true', host.innerHTML);
  check('hydration marks the rendered host', host.classes.has('md-mermaid-host--rendered'));
  check('hydration adds the always-visible toolbar row', host.innerHTML.includes('class="md-mermaid-toolbar"'), host.innerHTML);
  check('hydration toolbar carries View + Download SVG',
    host.innerHTML.includes('data-mermaid-action="view"') && host.innerHTML.includes('data-mermaid-action="download"')
      && host.innerHTML.includes('>View<') && host.innerHTML.includes('>Download SVG<'),
    host.innerHTML);
  check('hydration wraps the svg in its own scroller row',
    host.innerHTML.includes('class="md-mermaid-viewport"')
      && host.innerHTML.indexOf('md-mermaid-toolbar') < host.innerHTML.indexOf('<svg'),
    host.innerHTML);
} finally {
  mermaidMock.render = originalRender;
}

{
  mermaidSvgCacheTest.reset();
  const origRender = mermaidMock.render;
  mermaidMock.render = async () => ({
    svg: '<svg class="mock-svg" width="100%" style="max-width: 640px" viewBox="0 0 640 120"></svg>',
  });
  try {
    const author = fakeHost('flowchart LR\nA --> B\nclassDef foo fill:#f9f,stroke:#333\nclass A foo');
    await hydrateMermaid(fakeRoot([author]));
    const authorConfig = initState.config as { theme?: string; themeVariables?: Record<string, string | boolean> } | undefined;
    check('author-color mermaid initializes the default light theme', authorConfig?.theme === 'default', JSON.stringify(authorConfig));
    check('author-color mermaid does not apply dark themeVariables',
      authorConfig?.themeVariables === undefined
        || Object.keys(authorConfig.themeVariables).length === 0
        || authorConfig.themeVariables.primaryColor !== '#151820',
      JSON.stringify(authorConfig?.themeVariables));
    check('author-color host stamps palette and white canvas',
      author.getAttribute('data-mermaid-palette') === 'author'
        && author.innerHTML.includes(`style="background:#ffffff"`),
      author.innerHTML);
    check('author-color svg download surface is white',
      author.querySelector('svg')?.style.background === '#ffffff',
      author.innerHTML);
  } finally {
    mermaidMock.render = origRender;
    mermaidSvgCacheTest.reset();
  }
}

{
  // Fail-closed: a parsererror document (the '&nbsp;' sentinel) and a
  // missing DOMParser both reject the render output at the sanitize
  // boundary — the host degrades to the bounded source fallback with no
  // SVG and no viewer toolbar, never unsanitized markup.
  const origRender = mermaidMock.render;
  const prevParser = Reflect.get(globalThis, 'DOMParser');
  try {
    mermaidMock.render = async () => ({ svg: '<svg>&nbsp;</svg>' });
    const parseError = fakeHost('flowchart TD\nFAIL_PARSE --> B');
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

    Reflect.deleteProperty(globalThis, 'DOMParser');
    mermaidMock.render = async () => ({ svg: '<svg id="no-parser-0"></svg>' });
    const noParser = fakeHost('flowchart TD\nFAIL_NO_PARSER --> B');
    await hydrateMermaid(fakeRoot([noParser]));
    check('fail-closed: missing DOMParser degrades to the source fallback',
      noParser.innerHTML.includes('md-mermaid-error')
        && noParser.innerHTML.includes('Mermaid · source fallback'),
      noParser.innerHTML);
    check('fail-closed: missing-DOMParser fallback has no toolbar and no svg',
      !noParser.innerHTML.includes('md-mermaid-toolbar')
        && !noParser.innerHTML.includes('<svg'),
      noParser.innerHTML);
  } finally {
    mermaidMock.render = origRender;
    if (prevParser === undefined) Reflect.deleteProperty(globalThis, 'DOMParser');
    else Object.defineProperty(globalThis, 'DOMParser', { value: prevParser, configurable: true, writable: true });
  }
}

// ---------------------------------------------------------------------
// Zero-width host: a HIDDEN host (clientWidth 0 — closed fold/tab/offscreen
// mount) must NOT be classified wide — every diagram would be "wide" against
// width 0, forcing even a 120px diagram onto the horizontal scroller. The
// decision defers and re-evaluates EXACTLY ONCE on visible layout through a
// one-shot ResizeObserver. FakeResizeObserver mirrors the real observer's
// observable surface: one initial observation fires on observe().
// ---------------------------------------------------------------------
class FakeResizeObserver {
  static instances: FakeResizeObserver[] = [];
  callback: ResizeObserverCallback;
  disconnects = 0;
  constructor(callback: ResizeObserverCallback) {
    this.callback = callback;
    FakeResizeObserver.instances.push(this);
  }
  observe(): void {
    // The real observer always delivers one initial observation.
    this.callback([], this as unknown as ResizeObserver);
  }
  trigger(): void {
    // A disconnected real observer never delivers again.
    if (this.disconnects > 0) return;
    this.callback([], this as unknown as ResizeObserver);
  }
  disconnect(): void {
    this.disconnects += 1;
  }
  unobserve(): void {}
}
const prevResizeObserver = Reflect.get(globalThis, 'ResizeObserver');
Object.defineProperty(globalThis, 'ResizeObserver', { value: FakeResizeObserver, configurable: true, writable: true });

const originalWideRender = mermaidMock.render;
mermaidMock.render = async () => ({
  svg: '<svg class="mock-svg" width="100%" style="max-width: 640px" viewBox="0 0 640 120"></svg>',
});
try {
  // 1. Hidden host: the decision is deferred — no wide marking, no
  //    min-width, and exactly ONE recheck observer scheduled.
  const hidden = fakeHost('flowchart LR\nA --> B');
  hidden.clientWidth = 0;
  FakeResizeObserver.instances.length = 0;
  await hydrateMermaid(fakeRoot([hidden]));
  check('zero-width host defers the wide decision', hidden.querySelector('svg')?.getAttribute('data-mermaid-wide') === null, hidden.innerHTML);
  check('zero-width host recovers intrinsic width without wide marking or min-width',
    hidden.querySelector('svg')?.getAttribute('width') === '640' && !hidden.innerHTML.includes('min-width'),
    hidden.innerHTML);
  check('zero-width host schedules exactly one recheck observer',
    FakeResizeObserver.instances.length === 1, String(FakeResizeObserver.instances.length));

  // 2. Visible layout, diagram wider than the host: the recheck marks wide
  //    exactly once, disconnects, and a second trigger is inert.
  const recheck = FakeResizeObserver.instances[0];
  hidden.clientWidth = 320;
  recheck.trigger();
  check('recheck on visible layout marks the wide SVG', hidden.querySelector('svg')?.getAttribute('data-mermaid-wide') === 'true', hidden.innerHTML);
  check('recheck restores the intrinsic min-width', hidden.querySelector('svg')?.getAttribute('style') === 'min-width: 640px;', hidden.innerHTML);
  check('recheck disconnects after the decision', recheck.disconnects === 1, String(recheck.disconnects));
  recheck.trigger();
  check('recheck is inert after disconnecting', recheck.disconnects === 1, String(recheck.disconnects));

  // 3. Visible layout, diagram fits: the host gains a real positive width
  //    (800 > the 640 viewBox) BEFORE the recheck fires, so the deferred
  //    decision actually re-evaluates a fitting diagram — it stays centered
  //    (no wide marking, no forced min-width), the observer disconnects
  //    exactly once, and a second trigger is inert. A hidden host never
  //    permanently marks a narrow diagram wide and never leaks the deferred
  //    observer.
  const fitting = fakeHost('flowchart LR\nA --> B');
  fitting.clientWidth = 0;
  FakeResizeObserver.instances.length = 0;
  await hydrateMermaid(fakeRoot([fitting]));
  check('fitting recheck keeps the deferred host narrow', FakeResizeObserver.instances.length === 1, String(FakeResizeObserver.instances.length));
  const fittingRecheck = FakeResizeObserver.instances[0];
  fitting.clientWidth = 800;
  fittingRecheck.trigger();
  check('recheck leaves a fitting diagram centered (not wide)',
    fitting.querySelector('svg')?.getAttribute('data-mermaid-wide') === null,
    fitting.innerHTML);
  check('fitting recheck never forces a min-width',
    !(fitting.querySelector('svg')?.getAttribute('style') || '').includes('min-width')
      && !fitting.innerHTML.includes('min-width'),
    fitting.innerHTML);
  check('fitting recheck keeps the recovered intrinsic width',
    fitting.querySelector('svg')?.getAttribute('width') === '640',
    fitting.innerHTML);
  check('fitting recheck disconnects after the fitting decision', fittingRecheck.disconnects === 1, String(fittingRecheck.disconnects));
  fittingRecheck.trigger();
  check('fitting recheck is inert after disconnecting', fittingRecheck.disconnects === 1, String(fittingRecheck.disconnects));
} finally {
  mermaidMock.render = originalWideRender;
  if (prevResizeObserver === undefined) Reflect.deleteProperty(globalThis, 'ResizeObserver');
  else Object.defineProperty(globalThis, 'ResizeObserver', { value: prevResizeObserver, configurable: true, writable: true });
}

// Restore the environment: the shared SVG DOMParser shim only exists for
// this bundle's success paths.
if (prevDomParser === undefined) Reflect.deleteProperty(globalThis, 'DOMParser');
else Object.defineProperty(globalThis, 'DOMParser', { value: prevDomParser, configurable: true, writable: true });
if (failures > 0) process.exit(1);
console.log('mermaid presentation tests passed');
