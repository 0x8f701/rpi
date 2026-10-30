export type MdInline =
  | { type: 'text'; text: string }
  | { type: 'strong'; text: string }
  | { type: 'em'; text: string }
  | { type: 'code'; text: string }
  | { type: 'link'; text: string; href: string };

export type MdTableAlign = 'left' | 'center' | 'right';

export type MdBlock =
  | { type: 'p'; inlines: MdInline[] }
  | { type: 'h'; level: number; inlines: MdInline[] }
  | { type: 'li'; ordered: boolean; inlines: MdInline[] }
  | { type: 'quote'; inlines: MdInline[] }
  | { type: 'code'; lang: string; body: string }
  | { type: 'mermaid'; body: string }
  | { type: 'table'; align: MdTableAlign[]; header: MdInline[][]; rows: MdInline[][][] }
  | { type: 'hr' };

/** Schemes the transcript may render as a tappable link: https only for web
 *  targets (plain http is dropped, not merely warned about) plus mailto,
 *  matching the desktop shell's external-scheme set minus http. Everything
 *  else — javascript:, data:, intent:, file:, blob: — never becomes a link. */
export function safeMarkdownHref(raw: string): string {
  const url = raw.trim();
  if (/^(https|mailto):/i.test(url)) return url;
  return '';
}

/** Authority (host[:port]) of an absolute URL. Regex-based on purpose:
 *  React Native's URL polyfill throws on `hostname`. */
export function linkHost(href: string): string {
  const match = /^[a-z][a-z0-9+.-]*:\/\/([^/?#]*)/i.exec(href);
  return match ? match[1] : '';
}

/** Consent copy for launching an external link, naming the destination the
 *  way the desktop shell does. A URL with no authority (mailto:) reads as
 *  "this link" — the full URL is always shown. */
export function externalLinkPrompt(href: string): { title: string; message: string } {
  const host = linkHost(href);
  return {
    title: 'Open external link',
    message: `Open ${host === '' ? 'this link' : host} outside RpiChat?\n\nDestination: ${href}\n\nOnly continue if you recognise this address. A page can request this link without your intent.`,
  };
}

export function parseInlineMarkdown(raw: string): MdInline[] {
  const out: MdInline[] = [];
  let index = 0;
  while (index < raw.length) {
    const tick = raw.indexOf('`', index);
    const strong = raw.indexOf('**', index);
    const em = findEm(raw, index);
    const link = raw.indexOf('[', index);
    const next = minPositive(tick, strong, em, link);
    if (next === -1) {
      pushText(out, raw.slice(index));
      break;
    }
    if (next > index) pushText(out, raw.slice(index, next));
    if (next === tick) {
      const end = raw.indexOf('`', tick + 1);
      if (end < 0) {
        pushText(out, raw.slice(tick));
        break;
      }
      out.push({ type: 'code', text: raw.slice(tick + 1, end) });
      index = end + 1;
      continue;
    }
    if (next === strong) {
      const end = raw.indexOf('**', strong + 2);
      if (end < 0) {
        pushText(out, raw.slice(strong));
        break;
      }
      out.push({ type: 'strong', text: raw.slice(strong + 2, end) });
      index = end + 2;
      continue;
    }
    if (next === em) {
      const end = raw.indexOf('*', em + 1);
      if (end < 0) {
        pushText(out, raw.slice(em));
        break;
      }
      out.push({ type: 'em', text: raw.slice(em + 1, end) });
      index = end + 1;
      continue;
    }
    const close = raw.indexOf('](', link);
    const endParen = close >= 0 ? raw.indexOf(')', close + 2) : -1;
    if (close < 0 || endParen < 0) {
      pushText(out, raw.slice(link, link + 1));
      index = link + 1;
      continue;
    }
    const label = raw.slice(link + 1, close);
    const href = safeMarkdownHref(raw.slice(close + 2, endParen));
    if (!href) {
      pushText(out, label);
      index = endParen + 1;
      continue;
    }
    out.push({ type: 'link', text: label, href });
    index = endParen + 1;
  }
  return out.length > 0 ? out : [{ type: 'text', text: '' }];
}

function findEm(raw: string, from: number): number {
  let index = from;
  while (index < raw.length) {
    const at = raw.indexOf('*', index);
    if (at < 0) return -1;
    if (raw[at + 1] === '*') {
      index = at + 2;
      continue;
    }
    const end = raw.indexOf('*', at + 1);
    if (end > at + 1 && raw[end + 1] !== '*') return at;
    index = at + 1;
  }
  return -1;
}

function minPositive(...values: number[]): number {
  let best = -1;
  for (const value of values) {
    if (value < 0) continue;
    if (best < 0 || value < best) best = value;
  }
  return best;
}

function pushText(out: MdInline[], text: string): void {
  if (!text) return;
  const last = out[out.length - 1];
  if (last && last.type === 'text') {
    last.text += text;
    return;
  }
  out.push({ type: 'text', text });
}

/** Conservative thinking-body normalization: identity unless the WHOLE text
 *  has no real newline yet and holds at least two literal `\n` separators —
 *  a single backslash-n (e.g. inside a code snippet) is never rewritten. */
export function normalizeThinkingNewlines(text: string): string {
  if (text === '' || text.indexOf('\n') !== -1) return text;
  let literal = 0;
  for (let index = 0; index < text.length - 1; index += 1) {
    if (text[index] === '\\' && text[index + 1] === 'n') {
      literal += 1;
      index += 1;
    }
  }
  return literal >= 2 ? text.replace(/\\n/g, '\n') : text;
}

const TABLE_SEPARATOR_CELL = /^:?-+:?$/;

function splitTableRow(line: string): string[] {
  const cells: string[] = [];
  let cell = '';
  for (let index = 0; index < line.length; index += 1) {
    const char = line[index];
    if (char === '\\' && line[index + 1] === '|') {
      cell += '|';
      index += 1;
      continue;
    }
    if (char === '|') {
      cells.push(cell);
      cell = '';
      continue;
    }
    cell += char;
  }
  cells.push(cell);
  if (cells.length > 0 && cells[0].trim() === '') cells.shift();
  if (cells.length > 0 && cells[cells.length - 1].trim() === '') cells.pop();
  return cells.map((part) => part.trim());
}

function isTableSeparator(line: string): boolean {
  if (!line.includes('|')) return false;
  const cells = splitTableRow(line);
  return cells.length > 0 && cells.every((cell) => TABLE_SEPARATOR_CELL.test(cell));
}

function tableAlign(cell: string): MdTableAlign {
  const left = cell.startsWith(':');
  const right = cell.endsWith(':');
  if (left && right) return 'center';
  return right ? 'right' : 'left';
}

/** Header + separator + pipe rows. Rows are normalized to the header's width
 *  so cells and the alignment list stay index-aligned for the renderer. */
function tryTable(lines: string[], start: number): { block: MdBlock; consumed: number } | null {
  const headerLine = lines[start];
  if (headerLine === undefined || !headerLine.includes('|')) return null;
  const separatorLine = lines[start + 1];
  if (separatorLine === undefined || !isTableSeparator(separatorLine)) return null;
  const header = splitTableRow(headerLine);
  if (header.length === 0) return null;
  const separator = splitTableRow(separatorLine);
  const align = header.map((_, column) => tableAlign(separator[column] ?? ''));
  const rows: MdInline[][][] = [];
  let index = start + 2;
  while (index < lines.length) {
    const line = lines[index];
    if (line.trim() === '' || !line.includes('|')) break;
    const cells = splitTableRow(line);
    const row: MdInline[][] = [];
    for (let column = 0; column < header.length; column += 1) {
      row.push(parseInlineMarkdown(cells[column] ?? ''));
    }
    rows.push(row);
    index += 1;
  }
  return {
    block: { type: 'table', align, header: header.map((cell) => parseInlineMarkdown(cell)), rows },
    consumed: index - start,
  };
}

export function parseMarkdown(raw: string): MdBlock[] {
  const blocks: MdBlock[] = [];
  const fence = /```([^\n]*)\n([\s\S]*?)```/g;
  let last = 0;
  let match: RegExpExecArray | null;
  while ((match = fence.exec(raw)) !== null) {
    if (match.index > last) blocks.push(...parseFlow(raw.slice(last, match.index)));
    const lang = (match[1] ?? '').trim().toLowerCase();
    const body = match[2] ?? '';
    if (lang === 'mermaid') blocks.push({ type: 'mermaid', body });
    else blocks.push({ type: 'code', lang, body });
    last = match.index + match[0].length;
  }
  if (last < raw.length) blocks.push(...parseFlow(raw.slice(last)));
  return blocks.length > 0 ? blocks : [{ type: 'p', inlines: parseInlineMarkdown(raw) }];
}

function parseFlow(raw: string): MdBlock[] {
  const lines = raw.replace(/\r\n/g, '\n').split('\n');
  const blocks: MdBlock[] = [];
  let paragraph: string[] = [];
  const flush = () => {
    const text = paragraph.join('\n').trim();
    paragraph = [];
    if (text) blocks.push({ type: 'p', inlines: parseInlineMarkdown(text) });
  };
  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index];
    const table = tryTable(lines, index);
    if (table) {
      flush();
      blocks.push(table.block);
      index += table.consumed - 1;
      continue;
    }
    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      flush();
      blocks.push({ type: 'h', level: heading[1].length, inlines: parseInlineMarkdown(heading[2] ?? '') });
      continue;
    }
    if (/^\s*(---+|\*\*\*+)\s*$/.test(line)) {
      flush();
      blocks.push({ type: 'hr' });
      continue;
    }
    const quote = /^>\s?(.*)$/.exec(line);
    if (quote) {
      flush();
      blocks.push({ type: 'quote', inlines: parseInlineMarkdown(quote[1] ?? '') });
      continue;
    }
    const item = /^(\s*)([-*+]|\d+\.)\s+(.*)$/.exec(line);
    if (item) {
      flush();
      blocks.push({
        type: 'li',
        ordered: /^\d/.test(item[2] ?? ''),
        inlines: parseInlineMarkdown(item[3] ?? ''),
      });
      continue;
    }
    if (line.trim() === '') {
      flush();
      continue;
    }
    paragraph.push(line);
  }
  flush();
  return blocks;
}
