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

function explanationRegion(html: string): string {
  const at = html.indexOf('data-testid="file-dependency-map-relations"');
  if (at < 0) return '';
  const open = html.lastIndexOf('<div', at);
  const close = html.indexOf('</div>', at);
  if (open < 0 || close < 0) return '';
  return html.slice(open, close);
}

function listItems(html: string): string[] {
  return [...html.matchAll(/<li>([\s\S]*?)<\/li>/g)].map((match) => match[1]);
}

function edgeItems(region: string): string[] {
  const start = region.indexOf('<ul>');
  const end = region.indexOf('</ul>');
  if (start < 0 || end < 0) return [];
  return listItems(region.slice(start, end));
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
    language: 'type_script',
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
const directoryMap = {
  path: 'src',
  nodes: [node('src/a.ts', { declarations: [], purpose: [] }), node('src/b.ts', { declarations: [], purpose: [] })],
  outgoing: [{
    source: 'src/a.ts',
    line: 1,
    specifier: './b',
    kind: 'import',
    target: { type: 'file', path: 'src/b.ts' },
  }],
  incoming: [],
  coverage: coverage(),
};
check('directory graph accepts edges inside the folder', normalizeFileExplorerMap(directoryMap, 'src')?.outgoing.length === 1);
check('directory graph rejects a node outside the folder', normalizeFileExplorerMap({
  ...directoryMap,
  nodes: [...directoryMap.nodes, node('other.ts', { declarations: [], purpose: [] })],
}, 'src') === null);
check('workspace root graph accepts an empty path', normalizeFileExplorerMap({
  ...directoryMap,
  path: '',
  nodes: [node('a.ts', { declarations: [], purpose: [] })],
  outgoing: [],
}, '')?.path === '');

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
    || /^\s*[eupx]\d+\["/.test(line)
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
check('render is one diagram', !html.includes('file-explorer__map-node') && !html.includes('file-dependency-map-outgoing') && !html.includes('file-explorer__map-file'));
check('render does not invent a purpose', !html.includes('generated'));

const relationFixture = normalizeFileExplorerMap({
  path: 'src/alpha.ts',
  nodes: [
    {
      path: 'src/alpha.ts',
      language: 'type_script',
      parse: 'ok',
      declarations: [{ kind: 'fn', name: 'start', line: 8, doc: 'Starts the module.' }],
      purpose: [{ line: 2, text: 'Entry that wires the helper.' }],
    },
    {
      path: 'src/beta.ts',
      language: 'type_script',
      parse: 'ok',
      declarations: [{ kind: 'fn', name: 'help', line: 3, doc: null }],
      purpose: [],
    },
    node('src/caller.ts', { declarations: [], purpose: [] }),
  ],
  outgoing: [{
    source: 'src/alpha.ts',
    line: 1,
    specifier: './beta',
    kind: 'import',
    target: { type: 'file', path: 'src/beta.ts' },
  }, {
    source: 'src/alpha.ts',
    line: 4,
    specifier: 'left-pad',
    kind: 'import',
    target: { type: 'external', specifier: 'left-pad' },
  }, {
    source: 'src/alpha.ts',
    line: 5,
    specifier: 'crate::hidden',
    kind: 'use',
    target: { type: 'unresolved', reason: 'module ancestry is not available' },
  }],
  incoming: [{
    source: 'src/caller.ts',
    line: 9,
    specifier: './alpha',
    kind: 'import',
    target: { type: 'file', path: 'src/alpha.ts' },
  }],
  coverage: coverage({ incomplete: false, scanned_files: 2 }),
}, 'src/alpha.ts');
check('relation fixture parses the file edges and one incoming edge', relationFixture?.nodes.length === 3 && relationFixture?.outgoing.length === 3 && relationFixture?.incoming.length === 1);
const relationHtml = renderToStaticMarkup(createElement(FileDependencyMap, {
  map: relationFixture as FileExplorerMapResult,
  onOpenFile: () => {},
}));
const relationRegion = explanationRegion(relationHtml);
const relationEdges = edgeItems(relationRegion);
check('explanation lists each resolved direction once', relationEdges.length === 4 && relationEdges[0] === 'alpha.ts → beta.ts via import (line 1, ./beta)' && relationEdges[1] === 'alpha.ts → left-pad via import (line 4, left-pad) (external)' && relationEdges[2] === 'alpha.ts → module ancestry is not available via use (line 5, crate::hidden) (unresolved)' && relationEdges[3] === 'caller.ts → alpha.ts via import (line 9, ./alpha)');
const relationFacts = listItems(relationRegion.slice(relationRegion.indexOf('</ul>') + 5));
check('explanation keeps the source purpose and declaration', relationFacts.includes('alpha.ts purpose (line 2): Entry that wires the helper.') && relationFacts.includes('alpha.ts fn start (line 8): Starts the module.') && !relationFacts.some((fact) => fact.includes('purpose') && !fact.startsWith('alpha.ts')));
check('explanation does not invent a purpose for an empty purpose list', !relationRegion.includes('generated') && !relationRegion.includes('No purpose recorded'));
check('explanation does not repeat every node as a second inventory', !relationRegion.includes('file-explorer__map-node') && !relationRegion.includes('file-dependency-map-outgoing'));
check('relation explanation has an accessible name', relationRegion.includes('role="region"') && relationRegion.includes('aria-label="Resolved relationships"'));
check('relation model text stays escaped', (() => {
  const hostileRelation = normalizeFileExplorerMap({
    path: 'src/alpha.ts',
    nodes: [
      node('src/alpha.ts', { purpose: [{ line: 1, text: '<img src=x onerror=alert(1)>\n```mermaid\nflowchart TD\n  injected --&gt; other\n```' }], declarations: [] }),
      node('src/beta.ts', { purpose: [], declarations: [] }),
    ],
    outgoing: [{
      source: 'src/alpha.ts',
      line: 1,
      specifier: '<script>alert(1)</script>',
      kind: 'import',
      target: { type: 'file', path: 'src/beta.ts' },
    }],
    incoming: [],
    coverage: coverage(),
  }, 'src/alpha.ts');
  const hostileHtml = renderToStaticMarkup(createElement(FileDependencyMap, {
    map: hostileRelation as FileExplorerMapResult,
    onOpenFile: () => {},
  }));
  const diagram = hostileHtml.slice(hostileHtml.indexOf('md-mermaid-host'), hostileHtml.indexOf('file-dependency-map-relations'));
  const hosts = hostileHtml.match(/class="md-mermaid-host"/g) ?? [];
  const hostileRegion = explanationRegion(hostileHtml);
  return hosts.length === 1
    && diagram.includes('flowchart LR')
    && !diagram.includes('flowchart TD')
    && !diagram.includes('injected')
    && hostileRegion.includes('```mermaid')
    && hostileRegion.includes('&lt;img')
    && hostileRegion.includes('&lt;script&gt;')
    && !hostileRegion.includes('<img')
    && !hostileRegion.includes('<script>');
})());

const selfEdge = {
  source: 'src/alpha.ts',
  line: 3,
  specifier: './alpha',
  kind: 'import',
  target: { type: 'file', path: 'src/alpha.ts' },
};
const selfMap = normalizeFileExplorerMap({
  path: 'src/alpha.ts',
  nodes: [node('src/alpha.ts', { declarations: [], purpose: [] })],
  outgoing: [selfEdge],
  incoming: [selfEdge],
  coverage: coverage({ scanned_files: 1 }),
}, 'src/alpha.ts');
const selfEdges = edgeItems(explanationRegion(renderToStaticMarkup(createElement(FileDependencyMap, {
  map: selfMap as FileExplorerMapResult,
  onOpenFile: () => {},
}))));
check('self import recorded on both sides renders one row', selfEdges.length === 1 && selfEdges[0] === 'alpha.ts → alpha.ts via import (line 3, ./alpha)');
const distinctSelf = normalizeFileExplorerMap({
  path: 'src/alpha.ts',
  nodes: [node('src/alpha.ts', { declarations: [], purpose: [] })],
  outgoing: [selfEdge, { ...selfEdge, line: 11 }],
  incoming: [selfEdge],
  coverage: coverage({ scanned_files: 1 }),
}, 'src/alpha.ts');
const distinctEdges = edgeItems(explanationRegion(renderToStaticMarkup(createElement(FileDependencyMap, {
  map: distinctSelf as FileExplorerMapResult,
  onOpenFile: () => {},
}))));
check('a different self import line stays a separate row', distinctEdges.length === 2 && distinctEdges[0] === 'alpha.ts → alpha.ts via import (line 3, ./alpha)' && distinctEdges[1] === 'alpha.ts → alpha.ts via import (line 11, ./alpha)');

const isolated = normalizeFileExplorerMap({
  path: 'src',
  nodes: [
    node('src/lone.ts', { purpose: [], declarations: [] }),
    node('src/other.ts', { purpose: [], declarations: [] }),
  ],
  outgoing: [],
  incoming: [],
  coverage: coverage({ scanned_files: 2 }),
}, 'src');
const isolatedHtml = renderToStaticMarkup(createElement(FileDependencyMap, {
  map: isolated as FileExplorerMapResult,
  onOpenFile: () => {},
}));
const isolatedRegion = explanationRegion(isolatedHtml);
check('isolated nodes state that no relationships resolved', isolatedRegion.includes('No resolved relationships') && !isolatedRegion.includes('→') && !isolatedRegion.includes('generated'));

const rawBetaMap = {
  path: 'beta',
  nodes: [
    {
      path: 'beta/beta.ts',
      language: 'type_script',
      parse: 'ok',
      declarations: [{ kind: 'fn', name: 'ledger', line: 2, doc: 'Beta ledger.' }],
      purpose: [{ line: 2, text: 'Beta ledger.' }],
    },
    {
      path: 'beta/lib.ts',
      language: 'java_script',
      parse: 'ok',
      declarations: [],
      purpose: [],
    },
  ],
  outgoing: [{
    source: 'beta/beta.ts',
    line: 1,
    specifier: './lib',
    kind: 'import',
    target: { type: 'file', path: 'beta/lib.ts' },
  }],
  incoming: [],
  coverage: coverage({ scanned_files: 2, visible_entries_only: false, incomplete: false }),
};
const rawBeta = normalizeFileExplorerMap(rawBetaMap, 'beta');
check(
  'raw serde type_script normalizes to typescript',
  rawBeta?.nodes[0]?.language === 'typescript' && rawBeta.nodes[0]?.purpose[0]?.text === 'Beta ledger.',
);
check(
  'raw serde java_script normalizes to javascript',
  rawBeta?.nodes[1]?.language === 'javascript',
);
check(
  'obsolete raw typescript language rejects',
  normalizeFileExplorerMap({
    ...rawBetaMap,
    nodes: [{ ...rawBetaMap.nodes[0], language: 'typescript' }],
    outgoing: [],
  }, 'beta') === null,
);
check(
  'obsolete raw javascript language rejects',
  normalizeFileExplorerMap({
    ...rawBetaMap,
    nodes: [{ ...rawBetaMap.nodes[1], language: 'javascript' }],
    outgoing: [],
  }, 'beta') === null,
);
check(
  'unknown raw language rejects',
  normalizeFileExplorerMap({
    ...rawBetaMap,
    nodes: [{ ...rawBetaMap.nodes[0], language: 'coffeescript' }],
    outgoing: [],
  }, 'beta') === null,
);
for (const language of ['constructor', 'toString', '__proto__']) {
  check(
    `inherited language name ${language} rejects`,
    normalizeFileExplorerMap({
      ...rawBetaMap,
      nodes: [{ ...rawBetaMap.nodes[0], language }],
      outgoing: [],
    }, 'beta') === null,
  );
}

const outsideSolidity = {
  path: 'src',
  nodes: [
    node('src/A.sol', { language: 'solidity', declarations: [], purpose: [] }),
    node('src/nested/B.sol', { language: 'solidity', declarations: [], purpose: [] }),
  ],
  outgoing: [{
    source: 'src/A.sol',
    line: 2,
    specifier: 'oz/token/T.sol',
    kind: 'sol_import',
    target: { type: 'file', path: 'lib/oz/token/T.sol' },
  }],
  incoming: [],
  coverage: coverage(),
};
const outsideScope = normalizeFileExplorerMap(outsideSolidity, 'src');
const outsideTarget = outsideScope?.outgoing[0]?.target;
check(
  'directory graph keeps a workspace-relative file target outside the node set',
  outsideScope != null
    && outsideScope.path === 'src'
    && outsideScope.nodes.length === 2
    && outsideScope.nodes.every((item) => !item.path.startsWith('lib/'))
    && outsideScope.outgoing.length === 1
    && outsideTarget != null
    && outsideTarget.type === 'file'
    && outsideTarget.path === 'lib/oz/token/T.sol',
);
const outsideDiagram = outsideScope == null ? null : fileExplorerMapDiagram(outsideScope);
const outsideClickable = outsideDiagram == null ? [] : Object.values(outsideDiagram.filesById);
check(
  'outside file target is drawn and is not a clickable filesById path',
  outsideDiagram != null
    && !/^\s*click\b/m.test(outsideDiagram.source)
    && /^\s*x\d+\["lib\/oz\/token\/T\.sol"\]$/m.test(outsideDiagram.source)
    && /^\s*n0 -->\|"sol_import"\| x\d+$/m.test(outsideDiagram.source)
    && outsideClickable.length === 2
    && outsideClickable.every((path) => !path.startsWith('lib/')),
);
check(
  'adding the outside file as a node while the request is src rejects',
  normalizeFileExplorerMap({
    ...outsideSolidity,
    nodes: [
      ...outsideSolidity.nodes,
      node('lib/oz/token/T.sol', { language: 'solidity', declarations: [], purpose: [] }),
    ],
  }, 'src') === null,
);
check(
  'directory graph rejects an absolute file target outside the node set',
  normalizeFileExplorerMap({
    ...outsideSolidity,
    outgoing: [{
      ...outsideSolidity.outgoing[0],
      target: { type: 'file', path: '/lib/oz/token/T.sol' },
    }],
  }, 'src') === null,
);
check(
  'directory graph rejects a parent-jump file target',
  normalizeFileExplorerMap({
    ...outsideSolidity,
    outgoing: [{
      ...outsideSolidity.outgoing[0],
      target: { type: 'file', path: '../lib/oz/token/T.sol' },
    }],
  }, 'src') === null,
);
check(
  'directory graph rejects a raw parent segment that canonicalization would erase',
  normalizeFileExplorerMap({
    ...outsideSolidity,
    outgoing: [{
      ...outsideSolidity.outgoing[0],
      target: { type: 'file', path: 'contracts/src/../outside.sol' },
    }],
  }, 'src') === null,
);
check(
  'directory graph rejects a backslash parent segment before canonicalization',
  normalizeFileExplorerMap({
    ...outsideSolidity,
    outgoing: [{
      ...outsideSolidity.outgoing[0],
      target: { type: 'file', path: 'contracts\\src\\..\\outside.sol' },
    }],
  }, 'src') === null,
);
check(
  'directory graph rejects a raw windows drive absolute file target',
  normalizeFileExplorerMap({
    ...outsideSolidity,
    outgoing: [{
      ...outsideSolidity.outgoing[0],
      target: { type: 'file', path: 'C:\\tmp\\x' },
    }],
  }, 'src') === null,
);
check(
  'directory graph rejects a raw windows drive-relative file target',
  normalizeFileExplorerMap({
    ...outsideSolidity,
    outgoing: [{
      ...outsideSolidity.outgoing[0],
      target: { type: 'file', path: 'C:foo' },
    }],
  }, 'src') === null,
);
// Sol's design: the backend keeps a resolved same-directory file reference
// even when the one-level directory scan did not list that file as a node
// (the renderer already draws it as an unscanned x endpoint), so the
// normalizer no longer requires an in-directory file target to be a node.
// The safety negatives above (absolute, drive, parent-jump, node scan
// range, incoming ends) stay in force.
const scopedMissing = normalizeFileExplorerMap({
  ...outsideSolidity,
  outgoing: [{
    source: 'src/A.sol',
    line: 1,
    specifier: './nested/Missing.sol',
    kind: 'sol_import',
    target: { type: 'file', path: 'src/nested/Missing.sol' },
  }],
}, 'src');
const scopedMissingTarget = scopedMissing?.outgoing[0]?.target;
const scopedMissingDiagram = scopedMissing == null ? null : fileExplorerMapDiagram(scopedMissing);
const scopedMissingClickable = scopedMissingDiagram == null ? [] : Object.values(scopedMissingDiagram.filesById);
check(
  'directory graph keeps a safe in-directory file target outside the node set',
  scopedMissing != null
    && scopedMissingTarget != null
    && scopedMissingTarget.type === 'file'
    && scopedMissingTarget.path === 'src/nested/Missing.sol'
    && scopedMissingDiagram != null
    && !/^\s*click\b/m.test(scopedMissingDiagram.source)
    && /^\s*x\d+\["src\/nested\/Missing\.sol"\]$/m.test(scopedMissingDiagram.source)
    && /^\s*n0 -->\|"sol_import"\| x\d+$/m.test(scopedMissingDiagram.source)
    && scopedMissingClickable.length === 2
    && scopedMissingClickable.every((path) => path !== 'src/nested/Missing.sol'),
);

// Permanent regression for the real listener payload captured at
// /tmp/cr-graph-boundary-fixture.json (plain src/, no foundry.toml):
// one listing level, nodes only src/a.ts, and the resolved reference
// src/a.ts -> './nested/b' -> src/nested/b.ts stays a file target that is
// NOT a scanned node. The payload must be accepted and rendered with an
// unscanned x endpoint; filesById must never fabricate the unscanned file.
const realBoundaryMap = {
  path: 'src',
  nodes: [{
    path: 'src/a.ts',
    language: 'type_script',
    parse: 'ok',
    declarations: [{ kind: 'variable', name: 'alpha', line: 2, doc: null }],
    purpose: [],
  }],
  outgoing: [{
    source: 'src/a.ts',
    line: 1,
    specifier: './nested/b',
    kind: 'import',
    target: { type: 'file', path: 'src/nested/b.ts' },
  }],
  incoming: [],
  coverage: {
    scanned_files: 1,
    unsupported_files: 0,
    unreadable_files: 0,
    unreadable_directories: 0,
    incomplete_files: 0,
    dynamic_omitted: 0,
    truncated: false,
    visible_entries_only: true,
    incomplete: true,
  },
};
const realBoundary = normalizeFileExplorerMap(realBoundaryMap, 'src');
const realBoundaryTarget = realBoundary?.outgoing[0]?.target;
const realBoundaryDiagram = realBoundary == null ? null : fileExplorerMapDiagram(realBoundary);
const realBoundaryClickable = realBoundaryDiagram == null ? [] : Object.values(realBoundaryDiagram.filesById);
check(
  'real directory payload accepts src/a.ts referencing unscanned src/nested/b.ts',
  realBoundary != null
    && realBoundary.path === 'src'
    && realBoundary.nodes.length === 1
    && realBoundary.nodes[0]?.path === 'src/a.ts'
    && realBoundary.outgoing.length === 1
    && realBoundaryTarget != null
    && realBoundaryTarget.type === 'file'
    && realBoundaryTarget.path === 'src/nested/b.ts'
    && realBoundary.coverage.scannedFiles === 1
    && realBoundary.coverage.visibleEntriesOnly === true
    && realBoundary.coverage.incomplete === true,
);
check(
  'real directory payload draws the unscanned reference as an x endpoint, never a filesById entry',
  realBoundaryDiagram != null
    && !/^\s*click\b/m.test(realBoundaryDiagram.source)
    && /^\s*x\d+\["src\/nested\/b\.ts"\]$/m.test(realBoundaryDiagram.source)
    && /^\s*n0 -->\|"import"\| x\d+$/m.test(realBoundaryDiagram.source)
    && realBoundaryClickable.length === 1
    && realBoundaryClickable[0] === 'src/a.ts',
);
check(
  'directory incoming still rejects a file end outside the node set',
  normalizeFileExplorerMap({
    ...outsideSolidity,
    incoming: [{
      source: 'lib/oz/token/T.sol',
      line: 1,
      specifier: 'src/A.sol',
      kind: 'sol_import',
      target: { type: 'file', path: 'src/A.sol' },
    }],
  }, 'src') === null,
);
const rootSolidity = {
  path: '',
  nodes: [
    node('src/A.sol', { language: 'solidity', declarations: [], purpose: [] }),
    node('src/nested/B.sol', { language: 'solidity', declarations: [], purpose: [] }),
  ],
  outgoing: [{
    source: 'src/A.sol',
    line: 2,
    specifier: 'oz/token/T.sol',
    kind: 'sol_import',
    target: { type: 'file', path: 'lib/oz/token/T.sol' },
  }],
  incoming: [],
  coverage: coverage(),
};
const rootScope = normalizeFileExplorerMap(rootSolidity, '');
const rootTarget = rootScope?.outgoing[0]?.target;
check(
  'workspace root graph keeps src-only nodes and a lib file target',
  rootScope != null
    && rootScope.path === ''
    && rootScope.nodes.length === 2
    && rootScope.nodes.every((item) => item.path.startsWith('src/'))
    && rootScope.outgoing.length === 1
    && rootTarget != null
    && rootTarget.type === 'file'
    && rootTarget.path === 'lib/oz/token/T.sol',
);
const rootDiagram = rootScope == null ? null : fileExplorerMapDiagram(rootScope);
const rootClickable = rootDiagram == null ? [] : Object.values(rootDiagram.filesById);
check(
  'workspace root lib file target is drawn and is not a clickable filesById path',
  rootDiagram != null
    && !/^\s*click\b/m.test(rootDiagram.source)
    && /^\s*x\d+\["lib\/oz\/token\/T\.sol"\]$/m.test(rootDiagram.source)
    && /^\s*n0 -->\|"sol_import"\| x\d+$/m.test(rootDiagram.source)
    && rootClickable.length === 2
    && rootClickable.every((path) => path.startsWith('src/')),
);
const rootMissing = normalizeFileExplorerMap({
  ...rootSolidity,
  outgoing: [{
    source: 'src/A.sol',
    line: 1,
    specifier: './nested/Missing.sol',
    kind: 'sol_import',
    target: { type: 'file', path: 'src/nested/Missing.sol' },
  }],
}, '');
const rootMissingDiagram = rootMissing == null ? null : fileExplorerMapDiagram(rootMissing);
const rootMissingClickable = rootMissingDiagram == null ? [] : Object.values(rootMissingDiagram.filesById);
check(
  'workspace root accepts a safe relative missing file because it has no directory scope',
  rootMissing?.outgoing[0]?.target.path === 'src/nested/Missing.sol'
    && rootMissingDiagram != null
    && !/^\s*click\b/m.test(rootMissingDiagram.source)
    && /^\s*x\d+\["src\/nested\/Missing\.sol"\]$/m.test(rootMissingDiagram.source)
    && rootMissingClickable.length === 2
    && rootMissingClickable.every((path) => path !== 'src/nested/Missing.sol'),
);

// Frozen wire: MapLanguage gains `go` (serde snake_case) and MapTarget gains
// `package { dir }`. dir is canonically `.` (workspace root) or an
// already-canonical workspace-relative POSIX path. A package is an endpoint
// only: never a scanned node, never in filesById, never openable. Incoming
// edges stay file-target-only — a package target in incoming rejects.
const goServerMap = {
  path: 'src/server.go',
  nodes: [node('src/server.go', { language: 'go', declarations: [], purpose: [] })],
  outgoing: [{
    source: 'src/server.go',
    line: 3,
    specifier: 'example.com/proj/internal/store',
    kind: 'import',
    target: { type: 'package', dir: 'internal/store' },
  }],
  incoming: [],
  coverage: coverage({ incomplete: false }),
};
const goServer = normalizeFileExplorerMap(goServerMap, 'src/server.go');
const goServerTarget = goServer?.outgoing[0]?.target;
check(
  'go node parses and a subpackage target keeps its dir',
  goServer != null
    && goServer.nodes[0]?.language === 'go'
    && goServerTarget != null
    && goServerTarget.type === 'package'
    && goServerTarget.dir === 'internal/store',
);
check(
  'unknown raw golang alias rejects in the closed language set',
  normalizeFileExplorerMap({
    ...goServerMap,
    nodes: [{ ...goServerMap.nodes[0], language: 'golang' }],
    outgoing: [],
  }, 'src/server.go') === null,
);
const goServerDiagram = goServer == null ? null : fileExplorerMapDiagram(goServer);
const goServerClickable = goServerDiagram == null ? [] : Object.values(goServerDiagram.filesById);
check(
  'a multi-file package renders one endpoint labeled with the dir and never enters filesById',
  goServerDiagram != null
    && !/^\s*click\b/m.test(goServerDiagram.source)
    && /^\s*p0\["package internal\/store"\]$/m.test(goServerDiagram.source)
    && goServerDiagram.source.split('\n').filter((line) => /^\s*p\d+\["/.test(line)).length === 1
    && goServerClickable.length === 1
    && goServerClickable[0] === 'src/server.go',
);
check(
  'package root dir is the canonical dot and draws a dot endpoint',
  (() => {
    const root = normalizeFileExplorerMap({
      ...goServerMap,
      outgoing: [{
        source: 'src/server.go',
        line: 4,
        specifier: 'example.com/proj',
        kind: 'import',
        target: { type: 'package', dir: '.' },
      }],
    }, 'src/server.go');
    if (root?.outgoing[0]?.target.type !== 'package' || root.outgoing[0]?.target.dir !== '.') return false;
    const diagram = fileExplorerMapDiagram(root);
    return /^\s*p0\["package \."\]$/m.test(diagram.source)
      && Object.values(diagram.filesById).join(',') === 'src/server.go';
  })(),
);
check(
  'directory graph draws a package target outside the node set as an endpoint',
  (() => {
    const scoped = normalizeFileExplorerMap({
      path: 'src',
      nodes: [node('src/a.go', { language: 'go', declarations: [], purpose: [] })],
      outgoing: [{
        source: 'src/a.go',
        line: 1,
        specifier: 'example.com/proj/internal',
        kind: 'import',
        target: { type: 'package', dir: 'internal' },
      }],
      incoming: [],
      coverage: coverage({ incomplete: false }),
    }, 'src');
    if (scoped?.outgoing[0]?.target.type !== 'package') return false;
    const diagram = fileExplorerMapDiagram(scoped);
    return /^\s*p0\["package internal"\]$/m.test(diagram.source)
      && !/^\s*click\b/m.test(diagram.source)
      && Object.values(diagram.filesById).join(',') === 'src/a.go';
  })(),
);
check(
  'workspace root directory graph accepts a dot package endpoint',
  (() => {
    const root = normalizeFileExplorerMap({
      path: '',
      nodes: [node('main.go', { language: 'go', declarations: [], purpose: [] })],
      outgoing: [{
        source: 'main.go',
        line: 1,
        specifier: 'example.com/proj',
        kind: 'import',
        target: { type: 'package', dir: '.' },
      }],
      incoming: [],
      coverage: coverage({ incomplete: false }),
    }, '');
    if (root?.outgoing[0]?.target.type !== 'package') return false;
    const diagram = fileExplorerMapDiagram(root);
    return /^\s*p0\["package \."\]$/m.test(diagram.source)
      && Object.values(diagram.filesById).join(',') === 'main.go';
  })(),
);
check(
  'incoming package target rejects for a file map',
  normalizeFileExplorerMap({
    ...goServerMap,
    outgoing: [],
    incoming: [{
      source: 'src/client.go',
      line: 2,
      specifier: './server',
      kind: 'import',
      target: { type: 'package', dir: 'internal/store' },
    }],
  }, 'src/server.go') === null,
);
check(
  'incoming package target rejects for a directory map',
  normalizeFileExplorerMap({
    path: 'src',
    nodes: [node('src/a.go', { language: 'go', declarations: [], purpose: [] })],
    outgoing: [],
    incoming: [{
      source: 'src/a.go',
      line: 2,
      specifier: './store',
      kind: 'import',
      target: { type: 'package', dir: 'internal/store' },
    }],
    coverage: coverage({ incomplete: false }),
  }, 'src') === null,
);
for (const dir of ['', '..', '../escape', '/abs/pkg', 'C:go', 'src\\win', './rel', 'internal/store/', 'internal//store', ' src', 5, null, {}, true, undefined]) {
  check(
    `package dir ${JSON.stringify(dir) ?? 'undefined'} rejects`,
    normalizeFileExplorerMap({
      ...goServerMap,
      outgoing: [{
        source: 'src/server.go',
        line: 3,
        specifier: 'example.com/proj/internal/store',
        kind: 'import',
        target: { type: 'package', dir },
      }],
    }, 'src/server.go') === null,
  );
}
const pkgRender = renderToStaticMarkup(createElement(FileDependencyMap, {
  map: goServer as FileExplorerMapResult,
  onOpenFile: () => {},
}));
check(
  'relation row shows the package dir with the package qualifier',
  edgeItems(explanationRegion(pkgRender))[0]
    === 'server.go → internal/store via import (line 3, example.com/proj/internal/store) (package)',
);
check(
  'relation row for the root dot package shows the dot',
  (() => {
    const root = normalizeFileExplorerMap({
      ...goServerMap,
      outgoing: [{
        source: 'src/server.go',
        line: 4,
        specifier: 'example.com/proj',
        kind: 'import',
        target: { type: 'package', dir: '.' },
      }],
    }, 'src/server.go');
    if (root == null) return false;
    const html = renderToStaticMarkup(createElement(FileDependencyMap, {
      map: root as FileExplorerMapResult,
      onOpenFile: () => {},
    }));
    return edgeItems(explanationRegion(html))[0]
      === 'server.go → . via import (line 4, example.com/proj) (package)';
  })(),
);

console.log(`\nFileDependencyMap.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
