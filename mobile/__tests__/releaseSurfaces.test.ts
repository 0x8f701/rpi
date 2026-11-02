import { readFileSync } from 'fs';
import { join } from 'path';
import { parseMarkdown } from '../src/markdown';
import { mermaidViewerHtml } from '../src/mermaidSvg';
import { encodeTmuxB64, decodeTmuxB64, mapTmuxInputB64 } from '../src/tmuxCtrl';

const MERMAID_LOCK_INTEGRITY =
  'sha512-V6K3C8EBdEsPFZXSKMJe6ppQOENxuHARr9GvHX4hh47lAbhMRD9qf4oEK7LoaRQxULMa80/qt5gHO73aCleBBg==';

describe('mermaid offline lock and html', () => {
  it('pins mermaid 11.17.2 integrity on registry.npmjs.org', () => {
    const lock = JSON.parse(readFileSync(join(__dirname, '../package-lock.json'), 'utf8')) as {
      packages: Record<string, { version?: string; resolved?: string; integrity?: string }>;
    };
    const row = lock.packages['node_modules/mermaid'];
    expect(row).toBeTruthy();
    expect(row.version).toBe('11.17.2');
    expect(row.resolved).toBe('https://registry.npmjs.org/mermaid/-/mermaid-11.17.2.tgz');
    expect(row.integrity).toBe(MERMAID_LOCK_INTEGRITY);
  });

  it('loads mermaid.min.js from android_asset and never jsdelivr', () => {
    const html = mermaidViewerHtml('graph TD; A-->B;');
    expect(html.includes('cdn.jsdelivr.net')).toBe(false);
    expect(html.includes('src="mermaid.min.js"')).toBe(true);
    const svg = readFileSync(join(__dirname, '../src/mermaidSvg.ts'), 'utf8');
    expect(svg.includes('cdn.jsdelivr.net')).toBe(false);
    const gradle = readFileSync(join(__dirname, '../android/app/build.gradle'), 'utf8');
    expect(gradle.includes('node_modules/mermaid/dist/mermaid.min.js')).toBe(true);
    const blocks = parseMarkdown('```mermaid\ngraph TD; A-->B;\n```');
    expect(blocks.some((block) => block.type === 'mermaid' && block.body.includes('A-->B'))).toBe(true);
  });
});

describe('soft ctrl on encoded pty input', () => {
  it('turns a keyboard c into Ctrl+C only while sticky ctrl is held', () => {
    const encoded = encodeTmuxB64('c');
    const idle = mapTmuxInputB64(encoded, false);
    expect(idle.consumed).toBe(false);
    expect(idle.data).toBe(encoded);
    const held = mapTmuxInputB64(encoded, true);
    expect(held.consumed).toBe(true);
    expect(decodeTmuxB64(held.data)).toBe('\u0003');
    expect(mapTmuxInputB64(encodeTmuxB64('\u001b[A'), true).consumed).toBe(false);
  });
});
