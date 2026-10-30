// Test-only stand-in for `@xterm/xterm`, wired via esbuild `--alias`.
// Captures attach-time geometry, writes, onData, onResize, and dispose so
// chatTmuxTerminal.test.ts can drive the overlay without a real emulator.

export interface XtermSize {
  cols: number;
  rows: number;
}

export const xtermHarness: {
  cols: number;
  rows: number;
  writes: Array<string | Uint8Array>;
  scrollToBottomCalls: number;
  dataHandler: ((data: string) => void) | null;
  resizeHandler: ((size: XtermSize) => void) | null;
  scrollHandler: (() => void) | null;
  disposed: boolean;
  opened: boolean;
  lastTerm: Terminal | null;
  host: unknown;
  baseCols: number;
  baseRows: number;
  baseFontSize: number;
} = {
  cols: 120,
  rows: 36,
  writes: [],
  scrollToBottomCalls: 0,
  dataHandler: null,
  resizeHandler: null,
  scrollHandler: null,
  disposed: false,
  opened: false,
  lastTerm: null,
  host: null,
  baseCols: 120,
  baseRows: 36,
  baseFontSize: 0,
};

export function resetXtermHarness(size: XtermSize = { cols: 120, rows: 36 }): void {
  xtermHarness.cols = size.cols;
  xtermHarness.rows = size.rows;
  xtermHarness.writes = [];
  xtermHarness.scrollToBottomCalls = 0;
  xtermHarness.dataHandler = null;
  xtermHarness.resizeHandler = null;
  xtermHarness.scrollHandler = null;
  xtermHarness.disposed = false;
  xtermHarness.opened = false;
  xtermHarness.lastTerm = null;
  xtermHarness.host = null;
  xtermHarness.baseCols = size.cols;
  xtermHarness.baseRows = size.rows;
  xtermHarness.baseFontSize = 0;
}

function emitDefaultDaReply(term: Terminal, data: string | Uint8Array): void {
  const text = typeof data === 'string' ? data : new TextDecoder().decode(data);
  const queries: Array<{ prefix: string | undefined; reply: string; match: boolean }> = [
    { prefix: undefined, reply: '\x1b[?1;2c', match: text.includes('\x1b[c') },
    { prefix: '>', reply: '\x1b[>0;276;0c', match: /\x1b\[>\d*c/.test(text) },
  ];
  for (const query of queries) {
    if (!query.match) continue;
    const handled = term.csiHandlers.some((entry) => (
      entry.spec.final === 'c'
      && entry.spec.prefix === query.prefix
      && entry.handler([]) === true
    ));
    if (!handled) xtermHarness.dataHandler?.(query.reply);
  }
}

export class Terminal {
  cols = xtermHarness.cols;
  rows = xtermHarness.rows;
  options: Record<string, unknown>;
  csiHandlers: Array<{ spec: { prefix?: string; final?: string }; handler: (params: unknown) => boolean }> = [];
  parser: { registerCsiHandler: (spec: { prefix?: string; final?: string }, handler: (params: unknown) => boolean) => { dispose(): void } };

  constructor(options: Record<string, unknown> = {}) {
    this.options = options;
    this.cols = 80;
    this.rows = 24;
    this.csiHandlers = [];
    this.parser = {
      registerCsiHandler: (spec, handler) => {
        this.csiHandlers.push({ spec, handler });
        return { dispose() {} };
      },
    };
    xtermHarness.lastTerm = this;
    xtermHarness.writes = [];
    xtermHarness.scrollToBottomCalls = 0;
    xtermHarness.dataHandler = null;
    xtermHarness.resizeHandler = null;
    xtermHarness.scrollHandler = null;
    xtermHarness.disposed = false;
    xtermHarness.opened = false;
    if (xtermHarness.baseFontSize === 0) {
      xtermHarness.baseFontSize = Number(options.fontSize) || 15;
    }
  }

  loadAddon(_addon: unknown): void {}

  open(element: unknown): void {
    xtermHarness.opened = true;
    xtermHarness.host = element;
  }

  buffer = { active: { viewportY: 0, baseY: 0 } };

  element: { querySelector(_selector: string): { clientHeight: number } | null } | null = {
    querySelector() {
      return { clientHeight: 400 };
    },
  };

  onScroll(handler: () => void): { dispose(): void } {
    xtermHarness.scrollHandler = handler;
    return {
      dispose() {
        if (xtermHarness.scrollHandler === handler) xtermHarness.scrollHandler = null;
      },
    };
  }

  scrollToBottom(): void {
    xtermHarness.scrollToBottomCalls += 1;
  }

  scrollLines(_lines: number): void {}

  write(data: string | Uint8Array, callback?: () => void): void {
    xtermHarness.writes.push(data);
    emitDefaultDaReply(this, data);
    callback?.();
  }

  onData(handler: (data: string) => void): { dispose(): void } {
    xtermHarness.dataHandler = handler;
    return {
      dispose() {
        if (xtermHarness.dataHandler === handler) xtermHarness.dataHandler = null;
      },
    };
  }

  onResize(handler: (size: XtermSize) => void): { dispose(): void } {
    xtermHarness.resizeHandler = handler;
    return {
      dispose() {
        if (xtermHarness.resizeHandler === handler) xtermHarness.resizeHandler = null;
      },
    };
  }

  resize(cols: number, rows: number): void {
    this.cols = cols;
    this.rows = rows;
    xtermHarness.cols = cols;
    xtermHarness.rows = rows;
    xtermHarness.resizeHandler?.({ cols, rows });
  }

  focus(): void {}

  dispose(): void {
    xtermHarness.disposed = true;
    xtermHarness.dataHandler = null;
    xtermHarness.resizeHandler = null;
  }
}

export default { Terminal };
