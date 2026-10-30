// Test-only stand-in for `@xterm/addon-fit`. `fit()` scales the harness
// geometry by the current Terminal fontSize.

import { xtermHarness } from './xterm.ts';

export class FitAddon {
  proposeDimensions(): { cols: number; rows: number } {
    const fontSize = Number(xtermHarness.lastTerm?.options.fontSize);
    const baseFont = xtermHarness.baseFontSize || fontSize || 15;
    const size = Number.isFinite(fontSize) && fontSize > 0 ? fontSize : baseFont;
    return {
      cols: Math.max(1, Math.round(xtermHarness.baseCols * (baseFont / size))),
      rows: Math.max(1, Math.round(xtermHarness.baseRows * (baseFont / size))),
    };
  }

  fit(): void {
    const host = xtermHarness.host as { clientWidth?: number; clientHeight?: number } | null;
    if (host && ((host.clientWidth ?? 0) < 64 || (host.clientHeight ?? 0) < 24)) return;
    const dims = this.proposeDimensions();
    xtermHarness.lastTerm?.resize(dims.cols, dims.rows);
  }

  dispose(): void {}
}

export default { FitAddon };
