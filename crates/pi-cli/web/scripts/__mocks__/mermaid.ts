// Test-only stand-in for the `mermaid` module, wired via esbuild's
// `--alias:mermaid=./scripts/__mocks__/mermaid.ts` for the markdown
// unit-test bundle (see package.json build). hydrateMermaid needs
// initialize() to succeed and render() to resolve to an svg string for the
// success path; a source containing "BROKEN" makes render() reject so the
// error-degradation path (and its post-mutation callback) is exercised
// without a real browser.
export default {
  initialize(): void {
    // no-op: the real module configures theme/securityLevel, irrelevant here
  },
  async render(_id: string, source: string): Promise<{ svg: string }> {
    if (source.includes('BROKEN')) throw new Error('mock render failure');
    return { svg: '<svg class="mock-svg" xmlns="http://www.w3.org/2000/svg"></svg>' };
  },
};
