// Test-only stand-in for the `mermaid` module, wired via esbuild's
// `--alias:mermaid=./scripts/__mocks__/mermaid.ts` for the markdown
// unit-test bundle (see package.json build). hydrateMermaid needs
// initialize() to succeed and render() to resolve to an svg string for the
// success path; a source containing "BROKEN" makes render() reject so the
// error-degradation path (and its post-mutation callback) is exercised
// without a real browser.
//
// The stub also reproduces the mermaid 11.16.1 render side effect that the
// leak fix targets: the real mermaidAPI.render appends a temporary wrapper
// `<div id="d<id>"><svg id="<id>">…</svg></div>` to document.body and
// removes it on SUCCESS; on a PARSE error it throws BEFORE the removal,
// leaking the parser-error SVG ("Syntax error in text", "mermaid version
// …") into the page below the composer. When the repeated-failure
// regression test installs this module's `fakeDocument` as
// `globalThis.document`, the stub appends the same wrapper and keeps it on
// failure / removes it on success, so hydrateMermaid's per-render-id cleanup
// is observable in the unit suite.

interface FakeNode {
  id: string;
  textContent: string;
  remove(): void;
  querySelector(selector: string): FakeNode | null;
}

// Registry of appended temporary wrappers, keyed by element id. Mirrors the
// real document.body child list for the tests that install fakeDocument.
const tempWrappers: FakeNode[] = [];

const fakeDocument = {
  body: {
    appendChild(node: FakeNode): FakeNode {
      tempWrappers.push(node);
      return node;
    },
  },
  getElementById(id: string): FakeNode | null {
    return tempWrappers.find((node) => node.id === id) ?? null;
  },
};

/** The temp wrapper mermaid 11.16.1's appendDivSvgG creates for one render. */
function mermaidTempWrapper(id: string): FakeNode {
  const svg: FakeNode = {
    id,
    textContent: 'Syntax error in text mermaid version 11.16.1',
    remove() {
      // The svg is never registered on its own: it leaves the document with
      // its wrapper div (mermaid's removeTempElements removes `#d<id>`).
    },
    querySelector() {
      // The svg holds the drawn shapes; nothing nested carries a wrapper id.
      return null;
    },
  };
  const div: FakeNode = {
    id: `d${id}`,
    textContent: svg.textContent,
    remove() {
      const index = tempWrappers.indexOf(div);
      if (index >= 0) tempWrappers.splice(index, 1);
    },
    querySelector(selector: string): FakeNode | null {
      // hydrateMermaid's cleanup verifies the wrapper contains the svg drawn
      // for this render id before removing anything.
      if (selector !== `#${id}`) return null;
      return svg;
    },
  };
  return div;
}

export default {
  initialize(): void {
    // no-op: the real module configures theme/securityLevel, irrelevant here
  },
  async render(id: string, source: string): Promise<{ svg: string }> {
    // Only the repeated-failure regression installs fakeDocument as
    // globalThis.document; every other hydrate test has no document at all.
    // fakeDocument mirrors the real Document surface mermaid's render touches
    // (getElementById / body.appendChild), so this structural stand-in for
    // the browser Document is never reached with raw external input.
    const doc = globalThis.document as unknown as typeof fakeDocument | undefined;
    const wrapper = doc?.body ? doc.body.appendChild(mermaidTempWrapper(id)) : null;
    if (source.includes('BROKEN')) {
      // parse error: throw WITHOUT removing the wrapper, exactly like the
      // real mermaidAPI.render error path (removeTempElements is skipped)
      throw new Error('mock render failure');
    }
    wrapper?.remove(); // success: real mermaid removes the wrapper first
    return { svg: '<svg class="mock-svg" xmlns="http://www.w3.org/2000/svg"></svg>' };
  },
};

export { fakeDocument, tempWrappers };
