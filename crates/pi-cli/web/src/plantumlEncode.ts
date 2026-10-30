// PlantUML text encoding: raw deflate + PlantUML encode64.
// Spec: https://plantuml.com/text-encoding (deflate nowrap + 6-bit alphabet).
// No npm encoder. Browser CompressionStream('deflate-raw'); tests use node:zlib.

const PLANTUML_ENCODE64 =
  '0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz-_';

const PLANTUML_SERVER_MAX_CHARS = 2048;

export function encodePlantuml64(bytes: Uint8Array): string {
  let out = '';
  for (let i = 0; i < bytes.length; i += 3) {
    const b1 = bytes[i] ?? 0;
    const b2 = i + 1 < bytes.length ? bytes[i + 1]! : 0;
    const b3 = i + 2 < bytes.length ? bytes[i + 2]! : 0;
    out += PLANTUML_ENCODE64.charAt((b1 >> 2) & 0x3f);
    out += PLANTUML_ENCODE64.charAt(((b1 & 0x3) << 4 | (b2 >> 4)) & 0x3f);
    out += PLANTUML_ENCODE64.charAt(((b2 & 0xf) << 2 | (b3 >> 6)) & 0x3f);
    out += PLANTUML_ENCODE64.charAt(b3 & 0x3f);
  }
  return out;
}

export function parsePlantumlServer(raw: unknown): string | null {
  if (raw === null || raw === undefined) return '';
  if (typeof raw !== 'string') return null;
  const trimmed = raw.trim();
  if (trimmed === '') return '';
  if (trimmed.length > PLANTUML_SERVER_MAX_CHARS) return null;
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    return null;
  }
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return null;
  if (parsed.username !== '' || parsed.password !== '') return null;
  return trimmed.replace(/\/+$/, '');
}

export function plantumlSvgRequestUrl(server: string, encoded: string): string | null {
  const base = parsePlantumlServer(server);
  if (!base) return null;
  if (encoded === '' || /[^0-9A-Za-z\-_]/.test(encoded)) return null;
  try {
    return new URL(`svg/${encoded}`, `${base}/`).href;
  } catch {
    return null;
  }
}

export async function deflateRawPlantuml(source: string): Promise<Uint8Array> {
  if (typeof CompressionStream !== 'function') {
    throw new Error('deflate-raw is unavailable');
  }
  const stream = new Blob([source]).stream().pipeThrough(new CompressionStream('deflate-raw'));
  const buffer = await new Response(stream).arrayBuffer();
  return new Uint8Array(buffer);
}

export async function encodePlantumlSource(source: string): Promise<string> {
  const bytes = await deflateRawPlantuml(source);
  return encodePlantuml64(bytes);
}

/** Bounded diagnostic from a PlantUML server error SVG: syntax errors come
 *  back as HTTP 400 with the failing source line echoed in <text> nodes.
 *  Surfacing them lets the agent that wrote the diagram self-correct. */
export function plantumlHttpErrorDetail(status: number, body: string): string {
  const base = `plantuml HTTP ${status}`;
  if (status < 400) return base;
  const texts = Array.from(body.matchAll(/>([^<>]+)<\/text>/g), (m) => m[1]).map((t) =>
    t
      .replace(/&#(\d+);/g, (_, code) => {
        const n = Number(code);
        return n >= 0 && n <= 0x10FFFF ? String.fromCodePoint(n) : '';
      })
      .replace(/&#x([0-9a-fA-F]+);/g, (_, code) => {
        const n = parseInt(code, 16);
        return n >= 0 && n <= 0x10FFFF ? String.fromCodePoint(n) : '';
      })
      .replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&amp;/g, '&')
      .replace(/\s+/g, ' ').trim(),
  ).filter((t) => t !== '' && t !== '&#160;');
  if (texts.length === 0) return base;
  if (texts.some((t) => t.startsWith('Welcome to PlantUML'))) {
    return `${base}: the server could not decode the diagram payload`;
  }
  const errorIdx = texts.findIndex((t) => t.startsWith('Syntax Error'));
  if (errorIdx < 1) return base;
  const failing = texts[errorIdx - 1];
  const lineNo = texts.slice(0, errorIdx).reduce<string | null>((found, t) => {
    const m = t.match(/^\[From string \(line (\d+)\)/);
    return m ? m[1] : found;
  }, null);
  const where = lineNo !== null ? ` at line ${lineNo}` : '';
  const detail = `${texts[errorIdx]}${where}: ${failing}`;
  const room = 240 - base.length - 2;
  return detail.length <= room ? `${base}: ${detail}` : `${base}: ${detail.slice(0, room)}...`;
}

export function plantumlDocumentSource(source: string): string {
  const trimmed = source.trim();
  if (/^@start/i.test(trimmed)) return source;
  return `@startuml\n${source}\n@enduml\n`;
}

let plantumlServer = '';
let plantumlServerListener: ((server: string) => void) | null = null;

export function configuredPlantumlServer(): string {
  return plantumlServer;
}

export function subscribePlantumlServer(listener: (server: string) => void): void {
  plantumlServerListener = listener;
}

export function configurePlantumlServer(raw: unknown): void {
  const parsed = parsePlantumlServer(raw);
  const next = parsed === null ? '' : parsed;
  const changed = next !== plantumlServer;
  plantumlServer = next;
  if (changed) plantumlServerListener?.(next);
}

export function applyPlantumlServerFromCatalog(snapshot: {
  values?: Array<{ definition?: { key?: string }; effectiveValue?: unknown }>;
} | null | undefined): void {
  if (!Array.isArray(snapshot?.values)) return;
  const view = snapshot.values.find((row) => row?.definition?.key === 'plantumlServer');
  if (!view) return;
  configurePlantumlServer(view.effectiveValue);
}
