#!/usr/bin/env node
// Current-file dependency map: frozen wire parse, stale-request rejection,
// and a render that never interpolates a path into Mermaid click/HTML.
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import { FileDependencyMap } from '../src/panels/FileDependencyMap.tsx';
import {
  fileExplorerMapCommand,
  fileExplorerMapDiagram,
  fileExplorerMapMermaidLabel,
  fileExplorerMapResponseCurrent,
  normalizeFileExplorerMap,
  type FileExplorerMapResult,
} from '../src/fileExplorer.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: unknown, detail = ''): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function coverage(overrides: Record<string, unknown> = {}) {
  return {
    scanned_files: 2,
    unsupported_files: 0,
    unreadable_files: 0,
    unreadable_directories: 0,
    incomplete_files: 0,
    dynamic_omitted: 0,
    truncated: false,
    visible_entries_only: true,
    incomplete: true,
    ...overrides,
  };
}

function node(path: string, extras: Record<string, unknown> = {}) {
  return {
    path,
    language: 'typescript',
    parse: 'ok',
    declarations: [{ kind: 'fn', name: 'load', line: 4, doc: 'Loads the file.' }],
    purpose: [{ line: 4, text: 'Loads the file.' }],
    ...extras,
  };
}

const map = {
  path: 'src/app.ts',
  nodes: [
    node('src/app.ts'),
    node('src/util.ts', { declarations: [], purpose: [] }),
  ],
  outgoing: [{
    source: 'src/app.ts',
    line: 1,
    specifier: './util',
    kind: 'import',
    target: { type: 'file', path: 'src/util.ts' },
  }, {
    source: 'src/app.ts',
    line: 2,
    specifier: 'react',
    kind: 'import',
    target: { type: 'external', specifier: 'react' },
  }, {
    source: 'src/app.ts',
    line: 3,
    specifier: 'crate::hidden',
    kind: 'use',
    target: { type: 'unresolved', reason: 'Rust module ancestry is not available' },
  }],
  incoming: [{
    source: 'src/util.ts',
    line: 9,
    specifier: './app',
    kind: 'import',
    target: { type: 'file', path: 'src/app.ts' },
  }],
  coverage: coverage(),
};

check('map command is path only', JSON.stringify(fileExplorerMapCommand('src/app.ts')) === JSON.stringify({ type: 'file_explorer_map', path: 'src/app.ts' }));

const parsed = normalizeFileExplorerMap(map, 'src/app.ts');
check('canonical path matches the request', parsed?.path === 'src/app.ts' && parsed?.nodes.length === 2);
check('purpose stays the source doc', parsed?.nodes[0]?.purpose[0]?.text === 'Loads the file.' && parsed?.nodes[0]?.declarations[0]?.line === 4);
check('coverage stays incomplete and visible-only', parsed?.coverage.visibleEntriesOnly === true && parsed?.coverage.incomplete === true && parsed?.coverage.scannedFiles === 2);

check('path mismatch rejects the payload', normalizeFileExplorerMap(map, 'src/other.ts') === null);
check('canonical mismatch still matches', normalizeFileExplorerMap({ ...map, path: './src/app.ts' }, 'src/app.ts')?.path === './src/app.ts');
check('unknown target discriminant rejects', normalizeFileExplorerMap({
  ...map,
  outgoing: [{ ...map.outgoing[0], target: { type: 'guess', path: 'src/util.ts' } }],
}, 'src/app.ts') === null);
check('incoming that does not target the file rejects', normalizeFileExplorerMap({
  ...map,
  incoming: [{ ...map.incoming[0], target: { type: 'file', path: 'src/util.ts' } }],
}, 'src/app.ts') === null);
check('hub-file payload inside the backend bound is accepted', (() => {
  const hub = normalizeFileExplorerMap({
    ...map,
    nodes: [node('src/app.ts'), ...Array.from({ length: 520 }, (_, index) => node(`src/n${index}.ts`, { declarations: [], purpose: [] }))],
  }, 'src/app.ts');
  return hub != null && hub.nodes.length === 521;
})());
check('payload above the backend 256KiB bound rejects', normalizeFileExplorerMap({
  ...map,
  nodes: [node('src/app.ts'), ...Array.from({ length: 4000 }, (_, index) => node(`src/n${index}.ts`, { declarations: [], purpose: [] }))],
}, 'src/app.ts') === null);
check('cyclic payload fails closed', (() => {
  const cyclic: Record<string, unknown> = { ...map };
  cyclic.self = cyclic;
  return normalizeFileExplorerMap(cyclic, 'src/app.ts') === null;
})());
check('missing selected node rejects', normalizeFileExplorerMap({ ...map, nodes: [node('src/util.ts')] }, 'src/app.ts') === null);

const hostile = 'src/<img onerror=alert(1)>.ts" click n0 call evil()';
const hostileMap = normalizeFileExplorerMap({
  ...map,
  nodes: [node('src/app.ts'), node(hostile, { declarations: [], purpose: [] })],
  outgoing: [{
    source: 'src/app.ts',
    line: 1,
    specifier: hostile,
    kind: 'import',
    target: { type: 'external', specifier: hostile },
  }],
  incoming: [],
}, 'src/app.ts');
check('malicious label parses as text', hostileMap?.nodes[1]?.path === hostile);
const label = fileExplorerMapMermaidLabel(hostile);
check('malicious label drops quotes and brackets', !label.includes('"') && !label.includes('<') && !label.includes('>'));
const diagram = fileExplorerMapDiagram(hostileMap as FileExplorerMapResult);
check('diagram ids are opaque', diagram.source.includes('n0') && diagram.source.includes('n1') && diagram.filesById.n1 === hostile);
check('diagram grammar admits only node and edge lines, no directive statements', (() => {
  const lines = diagram.source.split('\n');
  const structural = lines.every((line) =>
    line === 'flowchart LR'
    || /^\s*n\d+\["/.test(line)
    || /^\s*[eu]\d+\["/.test(line)
    || /^\s*n\d+ -->\|"/.test(line));
  return structural
    && !lines.some((line) => /^\s*click\b/.test(line))
    && lines.some((line) => line.includes('click n0 call evil()'))
    && !diagram.source.includes(hostile)
    && !diagram.source.includes('<img');
})());

const request = { ownerKey: 'sess', path: 'src/app.ts', generation: 4 };
check('current request accepts', fileExplorerMapResponseCurrent(request, { ownerKey: 'sess', path: 'src/app.ts', generation: 4 }));
check('stale generation rejects', !fileExplorerMapResponseCurrent(request, { ownerKey: 'sess', path: 'src/app.ts', generation: 5 }));
check('stale owner rejects', !fileExplorerMapResponseCurrent(request, { ownerKey: 'other', path: 'src/app.ts', generation: 4 }));
check('stale path rejects', !fileExplorerMapResponseCurrent(request, { ownerKey: 'sess', path: 'src/other.ts', generation: 4 }));
check('cleared path rejects', !fileExplorerMapResponseCurrent(request, { ownerKey: 'sess', path: null, generation: 4 }));

const opened: string[] = [];
const html = renderToStaticMarkup(createElement(FileDependencyMap, {
  map: parsed as FileExplorerMapResult,
  onOpenFile: (path: string) => { opened.push(path); },
}));
check('render keeps mermaid fence and opaque ids', html.includes('flowchart LR') && html.includes('n0') && !html.includes('click '));
check('render separates external and unresolved', html.includes('external react') && html.includes('unresolved crate::hidden'));
check('render shows declaration and doc line', html.includes('fn load:4') && html.includes(':4 Loads the file.'));
check('render discloses incomplete coverage', html.includes('Visible workspace entries only') && html.includes('Coverage is incomplete.') && !html.includes('no dependencies'));
check('node list is a file button', html.includes('file-explorer__map-file') && html.includes('src/util.ts'));
check('render does not invent a purpose', !html.includes('generated') && html.includes('doc:4'));

console.log(`\nFileDependencyMap.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
