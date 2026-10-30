// Mermaid diagram viewer: per-host toolbar + one global full-screen viewer.
//
// Every successfully rendered `.md-mermaid-host` gains an always-visible
// toolbar row (`MERMAID_HOST_TOOLBAR_HTML`, applied by markdown.ts's
// applyRenderedMermaidSvg) with `View` and `Download SVG`; a double-click on
// the rendered SVG is a View shortcut. Both routes feed ONE global full-screen
// viewer (single lazily-built DOM singleton): Fit default, −/+/Fit/100%/
// Download/Close toolbar, pointer drag + pinch pan/zoom, wheel zoom, arrow-key
// pan and +/−/0/Escape keys, a focus trap that restores focus on close, and
// touch controls (Pointer Events + `touch-action: none` on the stage).
//
// Security posture: the host SVG only ever comes from mermaid.render() under
// `securityLevel: 'strict'` (markdown.ts — never disabled), but the viewer and
// the SVG download NEVER trust that output blindly. `sanitizeMermaidSvg()`
// re-parses the live instantiated SVG and strips `script`/`foreignObject`
// (and similar embed) elements, every `on*` event-handler attribute, any
// URL attribute (`href`/`xlink:href`/`src`/`data`) that is not a local `#`
// fragment, and CSS in `style` attributes / `<style>` content (every
// `@import` and every `url()` that is not a local `#fragment` is removed;
// local `url(#id)` refs are preserved). Without a DOMParser (plain-node)
// sanitize fails closed: the result is '' — nothing is shown or downloaded.
// The viewer additionally re-namespaces its private clone's IDs and every
// local ref (href, url(), ARIA IDREF, CSS id selectors) with a unique
// `mermaid-viewer-N-` prefix so the clone can never collide with the IDs of
// the host SVGs still live on the page. Download writes a vector
// `image/svg+xml` blob named `mermaid-diagram.svg`.
//
// No dependencies. All DOM work is guarded so importing this module in a
// plain-node test environment is side-effect free.

/** File name for every Mermaid SVG download (host toolbar and viewer). */
export const MERMAID_DOWNLOAD_FILE_NAME = 'mermaid-diagram.svg';

export type MermaidViewerPalette = 'theme' | 'author';

export const MERMAID_AUTHOR_CANVAS_FILL = '#ffffff';

/** Always-visible toolbar row appended to every successfully rendered host. */
export const MERMAID_HOST_TOOLBAR_HTML =
  '<div class="md-mermaid-toolbar">' +
  '<button type="button" class="md-mermaid-toolbar__btn" data-mermaid-action="view">View</button>' +
  '<button type="button" class="md-mermaid-toolbar__btn" data-mermaid-action="download">Download SVG</button>' +
  '</div>';

/* ------------------------------------------------------------------ *
 * Sanitization (viewer/export choke point)
 * ------------------------------------------------------------------ */

/** Elements removed outright before anything is displayed or downloaded. */
const MERMAID_SANITIZE_STRIP_ELEMENTS: Record<string, true> = {
  script: true,
  foreignobject: true,
  iframe: true,
  object: true,
  embed: true,
};

/** Attributes whose value must be a local `#fragment` or be dropped. */
const MERMAID_SANITIZE_URL_ATTRS: Record<string, true> = {
  href: true,
  'xlink:href': true,
  src: true,
  data: true,
};

/** Matches a CSS `url(...)` token, capturing the optional quote and target. */
const MERMAID_CSS_URL_RE = /url\(\s*(['"]?)([^'")]*)\1\s*\)/gi;

/** Matches a CSS `@import` rule through its terminating `;`, `}`, or end. */
const MERMAID_CSS_IMPORT_RE = /@import[^;}]*(?:;|}|$)/gi;

/**
 * Scrub CSS text (style attributes and <style> content): drop every
 * `@import` and every `url()` whose target is not a local `#fragment`.
 * Local `url(#id)` refs are preserved as-is (the viewer namespacer rewrites
 * them against its clone IDs).
 */
function sanitizeMermaidCss(css: string): string {
  let out = css.replace(MERMAID_CSS_IMPORT_RE, '');
  out = out.replace(MERMAID_CSS_URL_RE, (match, _quote, target: string) =>
    target.trim().startsWith('#') ? match : '');
  return out;
}

function mermaidSvgElements(root: Element): Element[] {
  const out: Element[] = [root];
  if (typeof root.querySelectorAll === 'function') {
    out.push(...Array.from(root.querySelectorAll('*')));
  }
  return out;
}

function serializeMermaidSvg(root: Element): string {
  if (typeof root.outerHTML === 'string' && root.outerHTML !== '') return root.outerHTML;
  if (typeof XMLSerializer !== 'undefined') {
    return new XMLSerializer().serializeToString(root);
  }
  return '';
}

/**
 * Parse an SVG string into a sanitized-ready root element, or null when
 * nothing may be shown: non-string/non-SVG input, no DOMParser (plain-node —
 * sanitize FAILS CLOSED, never passes unscrubbed markup through), or a
 * browser-side parse failure.
 */
function parseMermaidSvgRoot(svg: string): Element | null {
  if (typeof svg !== 'string' || !svg.includes('<svg')) return null;
  if (typeof DOMParser === 'undefined') return null;
  let root: Element;
  try {
    root = new DOMParser().parseFromString(svg, 'image/svg+xml').documentElement;
  } catch {
    return null;
  }
  if (!root) return null;
  const rootName = (root.localName || root.nodeName || '').toLowerCase();
  if (rootName === 'parsererror' || (typeof root.querySelector === 'function' && root.querySelector('parsererror'))) {
    return null;
  }
  return root;
}

/** Strip elements, event handlers, non-local URL attributes, and CSS hazards in place. */
function stripMermaidSvgHazards(root: Element): void {
  for (const el of mermaidSvgElements(root)) {
    const name = (el.localName || el.nodeName || '').toLowerCase();
    if (MERMAID_SANITIZE_STRIP_ELEMENTS[name] === true) {
      el.remove();
      continue;
    }
    const attrs = el.attributes;
    if (!attrs || typeof attrs.item !== 'function') continue;
    for (let i = attrs.length - 1; i >= 0; i -= 1) {
      const attr = attrs.item(i);
      if (!attr) continue;
      const attrName = (attr.localName || attr.name || '').toLowerCase();
      if (attrName.startsWith('on')) {
        el.removeAttribute(attr.name);
        continue;
      }
      if (attrName === 'style') {
        el.setAttribute(attr.name, sanitizeMermaidCss(attr.value));
        continue;
      }
      if (MERMAID_SANITIZE_URL_ATTRS[attrName] === true) {
        const value = attr.value.trim();
        if (!value.startsWith('#')) el.removeAttribute(attr.name);
      }
    }
    if (name === 'style' && typeof el.textContent === 'string') {
      el.textContent = sanitizeMermaidCss(el.textContent);
    }
  }
}

/**
 * Sanitize an SVG string for viewer display / SVG export: drop
 * script/foreignObject-style elements, `on*` event-handler attributes, every
 * URL attribute that does not point at a local `#fragment`, and CSS hazards
 * in `style` attributes / `<style>` content (`@import` and non-local `url()`
 * removed; local `url(#id)` preserved). Local `#id` refs (mermaid markers,
 * clip paths) are preserved untouched. Sanitize fails closed: with no
 * DOMParser (plain-node) or on a parse failure the result is '' — nothing
 * unsanitized is ever shown or saved.
 */
export function sanitizeMermaidSvg(svg: string): string {
  const root = parseMermaidSvgRoot(svg);
  if (!root) return '';
  stripMermaidSvgHazards(root);
  return serializeMermaidSvg(root);
}

/* ------------------------------------------------------------------ *
 * Host wiring (delegated toolbar actions + double-click shortcut)
 * ------------------------------------------------------------------ */

interface MermaidTargetNode {
  closest(selectors: string): unknown;
  getAttribute(name: string): string | null;
  querySelector(selectors: string): unknown;
}

function asMermaidTarget(value: unknown): MermaidTargetNode | null {
  if (!value || typeof value !== 'object') return null;
  const node = value as Partial<MermaidTargetNode>;
  if (typeof node.closest !== 'function' || typeof node.getAttribute !== 'function') return null;
  return node as MermaidTargetNode;
}

/** The live instantiated SVG markup inside a rendered host, or null. */
function mermaidHostSvgMarkup(host: Element): string | null {
  const svg = asMermaidTarget(host.querySelector('svg'));
  if (!svg) return null;
  const markup = (svg as { outerHTML?: string }).outerHTML;
  if (typeof markup === 'string' && markup.includes('<svg')) return markup;
  return null;
}

/**
 * Route a delegated click target to a host toolbar action. Only successful
 * hosts (`.md-mermaid-host` containing an SVG) route; everything else —
 * including error/fallback hosts — is ignored.
 */
export function mermaidViewerActionFromTarget(
  target: unknown,
): { action: 'view' | 'download'; host: Element } | null {
  const node = asMermaidTarget(target);
  if (!node) return null;
  const button = asMermaidTarget(node.closest('[data-mermaid-action]'));
  if (!button) return null;
  const action = button.getAttribute('data-mermaid-action');
  if (action !== 'view' && action !== 'download') return null;
  const host = asMermaidTarget(button.closest('.md-mermaid-host'));
  if (!host || !host.querySelector('svg')) return null;
  return { action, host: host as unknown as Element };
}

/**
 * Route a double-click target to the viewer: only a double-click on the SVG
 * of a rendered host opens it (`.md-mermaid-host--rendered svg`).
 */
export function dispatchMermaidViewerActivation(target: unknown): { host: Element } | null {
  const node = asMermaidTarget(target);
  if (!node) return null;
  const svg = asMermaidTarget(node.closest('svg'));
  if (!svg) return null;
  const host = asMermaidTarget(svg.closest('.md-mermaid-host--rendered'));
  if (!host) return null;
  return { host: host as unknown as Element };
}

/** Open the global viewer with a sanitized clone of the host's live SVG. */
export function openMermaidHostViewer(host: Element): void {
  const markup = mermaidHostSvgMarkup(host);
  const palette: MermaidViewerPalette = host.getAttribute('data-mermaid-palette') === 'author'
    ? 'author'
    : 'theme';
  if (markup !== null) {
    openMermaidViewer(markup, host.querySelector<HTMLButtonElement>('[data-mermaid-action="view"]'), palette);
  }
}

/** Download the host's live SVG (sanitized) as `mermaid-diagram.svg`. */
export function downloadMermaidHostSvg(host: Element): void {
  const markup = mermaidHostSvgMarkup(host);
  if (markup !== null) downloadMermaidSvg(sanitizeMermaidSvg(markup));
}

/* ------------------------------------------------------------------ *
 * SVG download (vector blob)
 * ------------------------------------------------------------------ */

/** Download a sanitized SVG string as a vector `mermaid-diagram.svg` blob. */
export function downloadMermaidSvg(svg: string): void {
  if (typeof document === 'undefined' || typeof URL?.createObjectURL !== 'function') return;
  if (typeof svg !== 'string' || svg === '') return;
  const blob = new Blob([svg], { type: 'image/svg+xml;charset=utf-8' });
  const objectUrl = URL.createObjectURL(blob);
  try {
    const anchor = document.createElement('a');
    anchor.href = objectUrl;
    anchor.download = MERMAID_DOWNLOAD_FILE_NAME;
    anchor.hidden = true;
    document.body.appendChild(anchor);
    anchor.click();
    anchor.remove();
  } finally {
    URL.revokeObjectURL(objectUrl);
  }
}

/* ------------------------------------------------------------------ *
 * One global full-screen viewer (lazy singleton)
 * ------------------------------------------------------------------ */

const MERMAID_VIEWER_MIN_SCALE = 0.05;
const MERMAID_VIEWER_MAX_SCALE = 40;
const MERMAID_VIEWER_ZOOM_STEP = 1.25;
const MERMAID_VIEWER_KEY_PAN = 48;

interface ViewerDom {
  root: HTMLDivElement;
  stage: HTMLDivElement;
  canvas: HTMLDivElement;
}

interface ViewerState {
  svgText: string;
  svgWidth: number;
  svgHeight: number;
  scale: number;
  tx: number;
  ty: number;
  restoreFocus: Element | null;
}

let viewerDom: ViewerDom | null = null;
let viewerState: ViewerState | null = null;
const viewerPointers = new Map<number, { x: number; y: number }>();
const viewerCapturedIds = new Set<number>();
let viewerPinchDistance = 0;


function applyViewerTransform(): void {
  if (!viewerState || !viewerDom) return;
  viewerDom.canvas.style.transform = `translate(${viewerState.tx}px, ${viewerState.ty}px) scale(${viewerState.scale})`;
}

function clampViewerScale(scale: number): number {
  if (!Number.isFinite(scale)) return MERMAID_VIEWER_MIN_SCALE;
  return Math.min(MERMAID_VIEWER_MAX_SCALE, Math.max(MERMAID_VIEWER_MIN_SCALE, scale));
}

/** Zoom to `nextScale` keeping the stage point (cx, cy) visually fixed. */
function viewerZoomAt(nextScale: number, cx: number, cy: number): void {
  if (!viewerState) return;
  const scale = clampViewerScale(nextScale);
  const ratio = scale / viewerState.scale;
  viewerState.tx = cx - (cx - viewerState.tx) * ratio;
  viewerState.ty = cy - (cy - viewerState.ty) * ratio;
  viewerState.scale = scale;
  applyViewerTransform();
}

function viewerZoomBy(factor: number): void {
  if (!viewerDom || !viewerState) return;
  const rect = viewerDom.stage.getBoundingClientRect();
  viewerZoomAt(viewerState.scale * factor, rect.width / 2, rect.height / 2);
}

/** Fit the diagram to the stage (viewer default) and center it. */
function viewerFit(): void {
  if (!viewerDom || !viewerState) return;
  const rect = viewerDom.stage.getBoundingClientRect();
  const fit = clampViewerScale(Math.min(rect.width / viewerState.svgWidth, rect.height / viewerState.svgHeight));
  viewerState.scale = fit;
  viewerState.tx = (rect.width - viewerState.svgWidth * fit) / 2;
  viewerState.ty = (rect.height - viewerState.svgHeight * fit) / 2;
  applyViewerTransform();
}

function viewerZoom100(): void {
  if (!viewerDom || !viewerState) return;
  const rect = viewerDom.stage.getBoundingClientRect();
  viewerZoomAt(1, rect.width / 2, rect.height / 2);
}

function viewerAction(action: string): void {
  if (action === 'zoom-in') viewerZoomBy(MERMAID_VIEWER_ZOOM_STEP);
  else if (action === 'zoom-out') viewerZoomBy(1 / MERMAID_VIEWER_ZOOM_STEP);
  else if (action === 'fit') viewerFit();
  else if (action === 'zoom-100') viewerZoom100();
  else if (action === 'download') {
    if (viewerState) downloadMermaidSvg(viewerState.svgText);
  } else if (action === 'close') closeMermaidViewer();
}

function viewerIntrinsicSize(svg: Element): { width: number; height: number } {
  const viewBox = svg.getAttribute('viewBox')?.trim().split(/[\s,]+/);
  if (viewBox?.length === 4) {
    const width = Number(viewBox[2]);
    const height = Number(viewBox[3]);
    if (Number.isFinite(width) && width > 0 && Number.isFinite(height) && height > 0) {
      return { width, height };
    }
  }
  const width = Number(svg.getAttribute('width'));
  const height = Number(svg.getAttribute('height'));
  if (Number.isFinite(width) && width > 0 && Number.isFinite(height) && height > 0) {
    return { width, height };
  }
  return { width: 800, height: 600 };
}

/** Focus trap: keep Tab cycling inside the viewer toolbar while open. */
function viewerTrapTab(event: KeyboardEvent): void {
  if (!viewerDom) return;
  const buttons = Array.from(viewerDom.root.querySelectorAll<HTMLButtonElement>('button[data-viewer-action]'))
    .filter((button) => !button.disabled);
  if (buttons.length === 0) return;
  const active = document.activeElement;
  const index = buttons.findIndex((button) => button === active);
  let next: number;
  if (event.shiftKey) next = index <= 0 ? buttons.length - 1 : index - 1;
  else next = index < 0 || index === buttons.length - 1 ? 0 : index + 1;
  event.preventDefault();
  buttons[next]?.focus();
}

function viewerKeydown(event: KeyboardEvent): void {
  if (!viewerState || !viewerDom) return;
  if (event.key === 'Tab') {
    viewerTrapTab(event);
    return;
  }
  if (event.key === 'Escape') {
    event.preventDefault();
    closeMermaidViewer();
    return;
  }
  const pan = MERMAID_VIEWER_KEY_PAN;
  if (event.key === 'ArrowLeft') viewerState.tx += pan;
  else if (event.key === 'ArrowRight') viewerState.tx -= pan;
  else if (event.key === 'ArrowUp') viewerState.ty += pan;
  else if (event.key === 'ArrowDown') viewerState.ty -= pan;
  else if (event.key === '+' || event.key === '=') {
    viewerZoomBy(MERMAID_VIEWER_ZOOM_STEP);
    return;
  } else if (event.key === '-' || event.key === '_') {
    viewerZoomBy(1 / MERMAID_VIEWER_ZOOM_STEP);
    return;
  } else if (event.key === '0') {
    viewerFit();
    return;
  } else {
    return;
  }
  applyViewerTransform();
  event.preventDefault();
}

/** The viewer's single stable document keydown handler (registered once). */
const viewerKeyHandler = (event: KeyboardEvent): void => viewerKeydown(event);

function viewerOnPointerMove(event: PointerEvent): void {
  if (!viewerState || !viewerDom) return;
  const previous = viewerPointers.get(event.pointerId);
  if (!previous) return;
  const rect = viewerDom.stage.getBoundingClientRect();
  const x = event.clientX - rect.left;
  const y = event.clientY - rect.top;
  if (viewerPointers.size >= 2) {
    // Two-finger pinch: zoom around the pointer midpoint.
    viewerPointers.set(event.pointerId, { x, y });
    const points = Array.from(viewerPointers.values());
    const distance = Math.hypot(points[0].x - points[1].x, points[0].y - points[1].y);
    const midX = (points[0].x + points[1].x) / 2;
    const midY = (points[0].y + points[1].y) / 2;
    if (viewerPinchDistance > 0 && distance > 0) viewerZoomAt(viewerState.scale * (distance / viewerPinchDistance), midX, midY);
    viewerPinchDistance = distance;
    return;
  }
  viewerState.tx += x - previous.x;
  viewerState.ty += y - previous.y;
  viewerPointers.set(event.pointerId, { x, y });
  applyViewerTransform();
}

function viewerOnPointerDown(event: PointerEvent): void {
  if (!viewerDom) return;
  const rect = viewerDom.stage.getBoundingClientRect();
  viewerPointers.set(event.pointerId, { x: event.clientX - rect.left, y: event.clientY - rect.top });
  viewerPinchDistance = 0;
  if (viewerPointers.size === 1) captureViewerPointer(event.pointerId);
}

function viewerOnPointerEnd(event: PointerEvent): void {
  releaseViewerPointerCapture(event.pointerId);
  viewerPointers.delete(event.pointerId);
  viewerPinchDistance = 0;
}

function captureViewerPointer(pointerId: number): void {
  if (!viewerDom) return;
  viewerDom.stage.setPointerCapture(pointerId);
  viewerCapturedIds.add(pointerId);
}

function releaseViewerPointerCapture(pointerId: number): void {
  const stage = viewerDom?.stage;
  viewerCapturedIds.delete(pointerId);
  if (!stage || typeof stage.releasePointerCapture !== 'function') return;
  if (typeof stage.hasPointerCapture === 'function' && !stage.hasPointerCapture(pointerId)) return;
  stage.releasePointerCapture(pointerId);
}

function releaseAllViewerPointerCaptures(): void {
  for (const pointerId of [...viewerCapturedIds]) {
    releaseViewerPointerCapture(pointerId);
  }
}

/* ------------------------------------------------------------------ *
 * Viewer clone ID namespacing (duplicate-ID + ref-integrity guard)
 * ------------------------------------------------------------------ */

/** Matches a local CSS `url(#id)` token (quote forms included). */
const MERMAID_URL_LOCAL_RE = /url\(\s*(['"]?)#([^'")]+)\1\s*\)/gi;

/** Cheap non-global probe: does this value carry a local `url(#... )` ref? */
const MERMAID_URL_LOCAL_HINT_RE = /url\(\s*['"]?#/i;

/** Matches a CSS id selector token: a `#id` compound at a selector position. */
const MERMAID_CSS_ID_SELECTOR_RE = /(^|[{}>~+,\s])#([A-Za-z_][-\w.]*)/g;

/** Hex-shaped tokens (`#fff`, `#aabbcc`) are colors, never id selectors. */
const MERMAID_CSS_HEX_COLOR_RE = /^(?:[0-9a-fA-F]{3,4}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})$/;

let mermaidViewerCloneSeq = 0;

/**
 * Rewrite local id refs in CSS text against the clone's renamed IDs: every
 * `url(#old)` and every id selector `#old` (when `old` is a renamed ID and
 * not hex-shaped) becomes `#mermaid-viewer-N-old`.
 */
function rewriteMermaidCssIdRefs(css: string, renamed: Map<string, string>): string {
  let out = css.replace(MERMAID_URL_LOCAL_RE, (match, quote: string, id: string) => {
    const next = renamed.get(id);
    return next === undefined ? match : `url(${quote}#${next}${quote})`;
  });
  out = out.replace(MERMAID_CSS_ID_SELECTOR_RE, (match, lead: string, id: string) => {
    const next = renamed.get(id);
    return next === undefined || MERMAID_CSS_HEX_COLOR_RE.test(id) ? match : `${lead}#${next}`;
  });
  return out;
}

/**
 * Re-namespace the sanitized viewer clone in place: every `id` gains the
 * unique `mermaid-viewer-N-` prefix and every local reference to it is
 * rewritten to match — `href`/`xlink:href` fragments, `url(#id)` inside any
 * attribute value (marker-end, clip-path, style, …), space-separated ARIA
 * IDREF lists, and CSS `url(#id)` + id selectors inside `<style>` content.
 * Host SVGs keep their original IDs, so the private clone can never
 * introduce a duplicate ID into the document.
 */
function namespaceMermaidViewerClone(root: Element): void {
  const prefix = `mermaid-viewer-${(mermaidViewerCloneSeq += 1)}-`;
  const renamed = new Map<string, string>();
  for (const el of mermaidSvgElements(root)) {
    const id = el.getAttribute('id');
    if (id !== null && id !== '' && !renamed.has(id)) renamed.set(id, prefix + id);
  }
  if (renamed.size === 0) return;
  for (const el of mermaidSvgElements(root)) {
    const attrs = el.attributes;
    if (!attrs || typeof attrs.item !== 'function') continue;
    const snapshot: { name: string; localName: string; value: string }[] = [];
    for (let i = 0; i < attrs.length; i += 1) {
      const attr = attrs.item(i);
      if (attr) snapshot.push({ name: attr.name, localName: attr.localName || attr.name || '', value: attr.value });
    }
    for (const { name, localName, value } of snapshot) {
      if (localName === 'id') {
        const next = renamed.get(value);
        if (next !== undefined && next !== value) el.setAttribute(name, next);
      } else if (localName === 'href' || localName === 'xlink:href') {
        const trimmed = value.trim();
        const next = trimmed.startsWith('#') ? renamed.get(trimmed.slice(1)) : undefined;
        if (next !== undefined) el.setAttribute(name, `#${next}`);
      } else if (localName === 'aria-labelledby' || localName === 'aria-describedby' || localName === 'aria-activedescendant') {
        el.setAttribute(name, value.trim().split(/\s+/).map((token) => renamed.get(token) ?? token).join(' '));
      } else if (localName === 'style' || MERMAID_URL_LOCAL_HINT_RE.test(value)) {
        el.setAttribute(name, rewriteMermaidCssIdRefs(value, renamed));
      }
    }
    if ((el.localName || el.nodeName || '').toLowerCase() === 'style' && typeof el.textContent === 'string') {
      el.textContent = rewriteMermaidCssIdRefs(el.textContent, renamed);
    }
  }
}

function ensureViewerDom(): ViewerDom {
  if (viewerDom) return viewerDom;
  const root = document.createElement('div');
  root.className = 'mermaid-viewer';
  root.setAttribute('role', 'dialog');
  root.setAttribute('aria-modal', 'true');
  root.setAttribute('aria-label', 'Mermaid diagram viewer');
  root.tabIndex = -1;
  root.hidden = true;
  root.innerHTML =
    '<div class="mermaid-viewer__toolbar">' +
    '<span class="mermaid-viewer__title">Mermaid diagram</span>' +
    '<button type="button" class="mermaid-viewer__btn" data-viewer-action="zoom-out" aria-label="Zoom out">−</button>' +
    '<button type="button" class="mermaid-viewer__btn" data-viewer-action="zoom-in" aria-label="Zoom in">+</button>' +
    '<button type="button" class="mermaid-viewer__btn" data-viewer-action="fit">Fit</button>' +
    '<button type="button" class="mermaid-viewer__btn" data-viewer-action="zoom-100">100%</button>' +
    '<button type="button" class="mermaid-viewer__btn" data-viewer-action="download">Download</button>' +
    '<button type="button" class="mermaid-viewer__btn" data-viewer-action="close">Close</button>' +
    '</div>' +
    '<div class="mermaid-viewer__stage"><div class="mermaid-viewer__canvas"></div></div>';
  document.body.appendChild(root);
  // ONE stable keydown handler for the viewer's whole lifetime: registered
  // once when the DOM is built, never removed, and a no-op while the viewer
  // is closed (viewerKeydown guards on viewerState). Reopening therefore
  // never stacks a second handler and close never leaks one.
  document.addEventListener('keydown', viewerKeyHandler);
  const stage = root.querySelector<HTMLDivElement>('.mermaid-viewer__stage') as HTMLDivElement;
  const canvas = root.querySelector<HTMLDivElement>('.mermaid-viewer__canvas') as HTMLDivElement;
  root.addEventListener('click', (event) => {
    const target = event.target as HTMLElement | null;
    const action = target?.closest?.('[data-viewer-action]')?.getAttribute('data-viewer-action');
    if (!action) return;
    event.preventDefault();
    viewerAction(action);
  });
  stage.addEventListener('wheel', (event) => {
    if (!viewerState) return;
    event.preventDefault();
    const rect = stage.getBoundingClientRect();
    const factor = Math.exp(-event.deltaY * 0.0015);
    viewerZoomAt(viewerState.scale * factor, event.clientX - rect.left, event.clientY - rect.top);
  }, { passive: false });
  stage.addEventListener('pointerdown', viewerOnPointerDown);
  stage.addEventListener('pointermove', viewerOnPointerMove);
  stage.addEventListener('pointerup', viewerOnPointerEnd);
  stage.addEventListener('pointercancel', viewerOnPointerEnd);
  stage.addEventListener('lostpointercapture', (event: PointerEvent) => {
    viewerCapturedIds.delete(event.pointerId);
    viewerPointers.delete(event.pointerId);
  });
  viewerDom = { root, stage, canvas };
  return viewerDom;
}

/** True while the global viewer is open (test seam, production-unused). */
export function isMermaidViewerOpen(): boolean {
  return viewerState !== null;
}

/**
 * Open the one global viewer with an SVG string. The input is parsed,
 * hazard-stripped, and its clone IDs re-namespaced first — a rejected input
 * leaves any already-open viewer untouched. Reopening while open reuses the
 * ONE stable keydown handler (registered once in ensureViewerDom, so no
 * listener stacks up) and keeps the ORIGINAL external restoreFocus target
 * instead of re-capturing focus from inside the viewer itself.
 */
export function openMermaidViewer(
  svgText: string,
  opener: Element | null = null,
  palette: MermaidViewerPalette = 'theme',
): void {
  if (typeof document === 'undefined' || typeof document.body?.appendChild !== 'function') return;
  const clone = parseMermaidSvgRoot(svgText);
  if (!clone) return;
  stripMermaidSvgHazards(clone);
  namespaceMermaidViewerClone(clone);
  const sanitized = serializeMermaidSvg(clone);
  if (sanitized === '') return;
  const { root, canvas } = ensureViewerDom();
  canvas.style.background = palette === 'author' ? MERMAID_AUTHOR_CANVAS_FILL : '';
  canvas.innerHTML = sanitized;
  const svg = canvas.firstElementChild;
  let width = 800;
  let height = 600;
  if (svg) {
    const intrinsic = viewerIntrinsicSize(svg);
    width = intrinsic.width;
    height = intrinsic.height;
  }
  const restoreFocus = viewerState !== null
    ? viewerState.restoreFocus
    : (opener ?? (document.activeElement instanceof Element ? document.activeElement : null));
  viewerState = { svgText: sanitized, svgWidth: width, svgHeight: height, scale: 1, tx: 0, ty: 0, restoreFocus };
  root.hidden = false;
  viewerFit();
  const close = root.querySelector<HTMLButtonElement>('button[data-viewer-action="close"]');
  close?.focus();
}

/**
 * Close the global viewer: clear the canvas and pointer state, hide the
 * dialog, and restore focus to the element that was focused when the viewer
 * was FIRST opened. The stable keydown handler stays registered — it no-ops
 * while the viewer is closed.
 */
export function closeMermaidViewer(): void {
  if (!viewerState || !viewerDom) return;
  const { root, canvas } = viewerDom;
  const { restoreFocus } = viewerState;
  viewerState = null;
  releaseAllViewerPointerCaptures();
  viewerPointers.clear();
  viewerPinchDistance = 0;
  canvas.innerHTML = '';
  canvas.style.background = '';
  root.hidden = true;
  if (restoreFocus && typeof (restoreFocus as HTMLElement).focus === 'function') {
    (restoreFocus as HTMLElement).focus();
  }
  if (root.contains(document.activeElement)) {
    (document.activeElement as HTMLElement).blur();
  }
}

/* ------------------------------------------------------------------ *
 * Delegated document listeners (every rendered markdown surface)
 * ------------------------------------------------------------------ */

if (typeof document !== 'undefined') {
  document.addEventListener('click', (event) => {
    const routed = mermaidViewerActionFromTarget(event.target);
    if (!routed) return;
    event.preventDefault();
    if (routed.action === 'view') openMermaidHostViewer(routed.host);
    else downloadMermaidHostSvg(routed.host);
  });
  document.addEventListener('dblclick', (event) => {
    const routed = dispatchMermaidViewerActivation(event.target);
    if (!routed) return;
    event.preventDefault();
    openMermaidHostViewer(routed.host);
  });
}