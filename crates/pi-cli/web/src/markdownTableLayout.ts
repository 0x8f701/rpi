export type TableCellWordBreak = 'normal' | 'break-all' | 'other';
export type TableCellOverflowWrap = 'anywhere' | 'break-word' | 'normal' | 'other';

export type TableCellWrapPolicy = {
  wordBreak: TableCellWordBreak;
  overflowWrap: TableCellOverflowWrap;
};

export const PHONE_TABLE_VIEWPORT = 390;

export const ASCII_PRIORITY_TOKENS = ['P0', 'P1', 'P2'] as const;

export function mdTableCellRule(css: string): string {
  const td = css.indexOf('.md-table td');
  if (td < 0) return '';
  const open = css.indexOf('{', td);
  const close = css.indexOf('}', open);
  if (open < 0 || close < 0 || close < open) return '';
  return css.slice(open + 1, close);
}

export function tableCellWrapPolicy(rule: string): TableCellWrapPolicy {
  const word = rule.match(/word-break:\s*([a-z-]+)/);
  const wrap = rule.match(/overflow-wrap:\s*([a-z-]+)/);
  return {
    wordBreak: parseWordBreak(word?.[1] ?? ''),
    overflowWrap: parseOverflowWrap(wrap?.[1] ?? ''),
  };
}

export function shippedTableCellsKeepAsciiTokens(policy: TableCellWrapPolicy): boolean {
  return policy.wordBreak === 'normal' && policy.overflowWrap === 'break-word';
}

export function mdTableFirstColumnRule(css: string): string {
  const needle = '.md-table tbody td:first-child';
  const td = css.indexOf(needle);
  if (td < 0) return '';
  const open = css.indexOf('{', td);
  const close = css.indexOf('}', open);
  if (open < 0 || close < 0 || close < open) return '';
  return css.slice(open + 1, close);
}

export function firstColumnKeepsCjkLabel(rule: string): boolean {
  return rule.includes('white-space: nowrap')
    && rule.includes('word-break: keep-all')
    && rule.includes('overflow-wrap: normal');
}

export function mdTableIsScrollWrap(css: string): boolean {
  const wrap = css.indexOf('.md-table-wrap');
  if (wrap < 0) return false;
  const open = css.indexOf('{', wrap);
  const close = css.indexOf('}', open);
  if (open < 0 || close < 0 || close < open) return false;
  const rule = css.slice(open + 1, close);
  const table = css.indexOf('.md-table {');
  if (table < 0) return false;
  const tableOpen = css.indexOf('{', table);
  const tableClose = css.indexOf('}', tableOpen);
  if (tableOpen < 0 || tableClose < 0) return false;
  const tableRule = css.slice(tableOpen + 1, tableClose);
  return rule.includes('overflow-x: auto')
    && tableRule.includes('display: table')
    && tableRule.includes('width: max-content');
}

export function phonePriorityColumnContentPx(opts: {
  viewportWidth: number;
  transcriptPadX: number;
  columns: number;
  cellPadX: number;
  borderPx: number;
  charPx: number;
  wordBreak: TableCellWordBreak;
  overflowWrap?: TableCellOverflowWrap;
  priorityToken: string;
}): number {
  const tableInner = Math.max(1, opts.viewportWidth - 2 * opts.transcriptPadX);
  const chromePer = 2 * opts.cellPadX + 2 * opts.borderPx;
  const minContentBreaks = opts.wordBreak === 'break-all' || opts.overflowWrap === 'anywhere';
  const wrapUnit = minContentBreaks ? 1 : Math.max(1, opts.priorityToken.length);
  const priorityMin = wrapUnit * opts.charPx + chromePer;
  const otherMin = 4 * opts.charPx + chromePer;
  const mins = [priorityMin, ...Array.from({ length: Math.max(0, opts.columns - 1) }, () => otherMin)];
  const minTotal = mins.reduce((sum, width) => sum + width, 0);
  const scale = minTotal > tableInner ? tableInner / minTotal : 1;
  return Math.max(1, mins[0]! * scale - chromePer);
}

export function asciiTokenLineCount(
  token: string,
  columnContentPx: number,
  charPx: number,
  policy: TableCellWrapPolicy,
): number {
  if (token.length === 0) return 0;
  const charsPerLine = Math.max(1, Math.floor(columnContentPx / charPx));
  if (policy.wordBreak === 'break-all') {
    return Math.ceil(token.length / charsPerLine);
  }
  if (token.length * charPx <= columnContentPx) return 1;
  if (policy.overflowWrap === 'anywhere') {
    return Math.ceil(token.length / charsPerLine);
  }
  return 1;
}

export function cjkTokenLineCount(text: string, columnContentPx: number, cjkPx: number): number {
  const chars = [...text];
  if (chars.length === 0) return 0;
  const charsPerLine = Math.max(1, Math.floor(columnContentPx / cjkPx));
  return Math.ceil(chars.length / charsPerLine);
}

function parseWordBreak(value: string): TableCellWordBreak {
  if (value === 'normal') return 'normal';
  if (value === 'break-all') return 'break-all';
  return 'other';
}

function parseOverflowWrap(value: string): TableCellOverflowWrap {
  if (value === 'anywhere') return 'anywhere';
  if (value === 'break-word') return 'break-word';
  if (value === 'normal') return 'normal';
  return 'other';
}
