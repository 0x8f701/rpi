// Directory dependency graph. One Mermaid fence rendered by the shared
// MarkdownBody. Node ids are opaque (`n0`...). Paths never enter click
// handlers or HTML.

import { MarkdownBody } from '../MarkdownBody';
import { safeText } from '../redact';
import {
  fileExplorerBasename,
  fileExplorerMapDiagram,
  type FileExplorerMapEdge,
  type FileExplorerMapResult,
} from '../fileExplorer';

export interface FileDependencyMapProps {
  map: FileExplorerMapResult;
  workspaceCwd?: string;
  onOpenFile: (path: string) => void;
}

function relationRows(map: FileExplorerMapResult): Array<{
  key: string;
  source: string;
  target: string;
  kind: FileExplorerMapEdge['kind'];
  line: number;
  specifier: string;
  qualifier: '' | 'external' | 'unresolved' | 'package';
}> {
  const rows: Array<{
    key: string;
    source: string;
    target: string;
    kind: FileExplorerMapEdge['kind'];
    line: number;
    specifier: string;
    qualifier: '' | 'external' | 'unresolved' | 'package';
  }> = [];
  const seen = new Set<string>();
  const push = (edge: FileExplorerMapEdge, index: number, direction: 'out' | 'in') => {
    const targetIdentity = edge.target.type === 'file'
      ? edge.target.path
      : edge.target.type === 'external'
        ? edge.target.specifier
        : edge.target.type === 'package'
          ? edge.target.dir
          : edge.target.reason;
    const identity = [edge.source, edge.target.type, targetIdentity, edge.kind, String(edge.line), edge.specifier].join('\0');
    if (seen.has(identity)) return;
    seen.add(identity);
    let target = '';
    let qualifier: '' | 'external' | 'unresolved' | 'package' = '';
    if (edge.target.type === 'file') {
      target = fileExplorerBasename(edge.target.path) || edge.target.path;
    } else if (edge.target.type === 'external') {
      target = edge.target.specifier;
      qualifier = 'external';
    } else if (edge.target.type === 'package') {
      target = edge.target.dir;
      qualifier = 'package';
    } else {
      target = edge.target.reason;
      qualifier = 'unresolved';
    }
    rows.push({
      key: `${direction}-${index}`,
      source: fileExplorerBasename(edge.source) || edge.source,
      target,
      kind: edge.kind,
      line: edge.line,
      specifier: edge.specifier,
      qualifier,
    });
  };
  map.outgoing.forEach((edge, index) => push(edge, index, 'out'));
  map.incoming.forEach((edge, index) => push(edge, index, 'in'));
  return rows;
}

export function FileDependencyMap({ map }: FileDependencyMapProps) {
  const diagram = fileExplorerMapDiagram(map);
  const markdown = '```mermaid\n' + diagram.source + '\n```';
  const coverage = map.coverage;
  const coverageNotes = [
    coverage.truncated ? 'Scan truncated.' : '',
    coverage.unsupportedFiles > 0 ? `${coverage.unsupportedFiles} unsupported.` : '',
    coverage.unreadableFiles > 0 ? `${coverage.unreadableFiles} unreadable files.` : '',
    coverage.unreadableDirectories > 0 ? `${coverage.unreadableDirectories} unreadable directories.` : '',
    coverage.incompleteFiles > 0 ? `${coverage.incompleteFiles} incomplete parses.` : '',
  ].filter((note) => note !== '');
  const rows = relationRows(map);
  const facts: string[] = [];
  for (const node of map.nodes) {
    const name = fileExplorerBasename(node.path) || node.path;
    for (const purpose of node.purpose) {
      facts.push(`${name} purpose (line ${purpose.line}): ${purpose.text}`);
    }
    for (const declaration of node.declarations) {
      const declared = declaration.name ? ` ${declaration.name}` : '';
      const doc = declaration.doc ? `: ${declaration.doc}` : '';
      facts.push(`${name} ${declaration.kind}${declared} (line ${declaration.line})${doc}`);
    }
  }
  return (
    <div className="file-explorer__map" data-testid="file-dependency-map" data-map-path={map.path}>
      <MarkdownBody className="file-explorer__map-diagram" text={markdown} />
      <div
        data-testid="file-dependency-map-relations"
        role="region"
        aria-label="Resolved relationships"
        tabIndex={0}
      >
        {rows.length === 0 ? (
          <p>No resolved relationships.</p>
        ) : (
          <ul>
            {rows.map((row) => (
              <li key={row.key}>
                {safeText(row.source)}
                {' \u2192 '}
                {safeText(row.target)}
                {' via '}
                {row.kind}
                {' (line '}
                {row.line}
                {', '}
                {safeText(row.specifier)}
                {')'}
                {row.qualifier === '' ? null : ` (${row.qualifier})`}
              </li>
            ))}
          </ul>
        )}
        {facts.length > 0 && (
          <ul>
            {facts.map((fact, index) => (
              <li key={index}>{safeText(fact)}</li>
            ))}
          </ul>
        )}
      </div>
      {coverageNotes.length > 0 && (
        <p className="file-explorer__map-coverage" data-testid="file-dependency-map-coverage">
          {safeText(coverageNotes.join(' '))}
        </p>
      )}
    </div>
  );
}
