import {
  cssBlock,
  rectsIntersect,
  type SheetBox,
} from './subagentSheetLayout';

export const CJK_TRANSCRIPT_TOOL =
  '对当前代码库做一次全面审查：核对工具卡片原始参数在窄屏下完整换行，且不得覆盖随后的思考块。'.repeat(4);

export function transcriptBlocksCanOverlap(css: string): boolean {
  const kids = cssBlock(css, '.transcript-content > * {');
  const card = cssBlock(css, '.tool-card {');
  const raw = cssBlock(css, '.tool-card__raw {');
  const args = cssBlock(css, '.tool-card__args {');
  const thinking = cssBlock(css, '.thinking {');
  const row = cssBlock(css, '.steering-queue__row {');
  if (/position:\s*(absolute|fixed)/.test(raw) || /position:\s*(absolute|fixed)/.test(args)) return true;
  if (/(?:^|[^\w-])(?:max-)?height:\s*\d/.test(raw) || /(?:^|[^\w-])(?:max-)?height:\s*\d/.test(args)) return true;
  if (/min-height:\s*0/.test(kids) || /min-height:\s*0/.test(card)) return true;
  if (/position:\s*(absolute|fixed)/.test(thinking)) return true;
  if (!/flex:\s*none/.test(kids) || !/flex:\s*none/.test(card)) return true;
  if (!css.includes('.transcript-content details:not([open]) > *:not(summary)')) return true;
  if (/(?:^|[^\w-])(?:max-)?height:\s*\d/.test(row) && !/overflow:\s*hidden/.test(row)) return true;
  return false;
}

export function outputFrameStealsRawArgs(css: string): boolean {
  const output = cssBlock(css, '.tool-card__output {');
  return /(?:^|[^\w-])border:\s*1px/.test(output);
}

export function assistantIndentsThinking(css: string): boolean {
  const asst = cssBlock(css, '.msg--assistant {');
  return !/padding-inline:\s*0/.test(asst);
}

export function rawArgsInsideCardBorder(card: SheetBox, raw: SheetBox): boolean {
  return raw.left >= card.left && raw.right <= card.right && raw.top >= card.top && raw.bottom <= card.bottom;
}

export function siblingLeftsAligned(a: SheetBox, b: SheetBox): boolean {
  return Math.abs(a.left - b.left) <= 0.5;
}

export function rawArgsOutsideVisualCard(opts: {
  cardBottom: number;
  outputBottom: number;
  rawTop: number;
  outputHasOwnBorder: boolean;
}): boolean {
  const visualBottom = opts.outputHasOwnBorder ? opts.outputBottom : opts.cardBottom;
  return opts.rawTop >= visualBottom - 0.5;
}

export function adjacentTranscriptRects(opts: {
  viewportWidth: number;
  padX: number;
  gap: number;
  toolContentHeight: number;
  thinkingHeight: number;
  toolShrinks: boolean;
  thinkingInset?: number;
}): { tool: SheetBox; thinking: SheetBox } {
  const left = opts.padX;
  const right = opts.viewportWidth - opts.padX;
  const toolBoxHeight = opts.toolShrinks
    ? Math.min(opts.toolContentHeight, 48)
    : opts.toolContentHeight;
  const tool: SheetBox = {
    left,
    right,
    top: 0,
    bottom: opts.toolContentHeight,
  };
  const thinkingTop = toolBoxHeight + opts.gap;
  const inset = opts.thinkingInset ?? 0;
  const thinking: SheetBox = {
    left: left + inset,
    right,
    top: thinkingTop,
    bottom: thinkingTop + opts.thinkingHeight,
  };
  return { tool, thinking };
}

export function phoneTranscriptAdjacentRects(css: string, toolText = CJK_TRANSCRIPT_TOOL) {
  const charsPerLine = Math.max(1, Math.floor((390 - 32) / 13));
  const lines = Math.ceil([...toolText].length / charsPerLine);
  return adjacentTranscriptRects({
    viewportWidth: 390,
    padX: 16,
    gap: 8,
    toolContentHeight: lines * 13 * 1.6,
    thinkingHeight: 72,
    toolShrinks: transcriptBlocksCanOverlap(css),
    thinkingInset: assistantIndentsThinking(css) ? 10 : 0,
  });
}

export { rectsIntersect };
