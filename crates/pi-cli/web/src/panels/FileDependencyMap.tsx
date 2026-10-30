// Current-file dependency map. The graph is a generated Mermaid fence
// rendered by the shared MarkdownBody (strict Mermaid + View/Download).
// Node ids are opaque (`n0`...). Paths never enter click handlers or HTML;
// file navigation is the existing explorer open path, from the node list.

import { MarkdownBody } from '../MarkdownBody';
import { safeText } from '../redact';
import {
  fileExplorerBasename,
  fileExplorerDisplayPath,
  fileExplorerMapDiagram,
  fileExplorerSamePath,
  type FileExplorerMapEdge,
  type FileExplorerMapNode,
  type FileExplorerMapResult,
} from '../fileExplorer';

export interface FileDependencyMapProps {
  map: FileExplorerMapResult;
  workspaceCwd?: string;
  onOpenFile: (path: string) => void;
}

function roleLine(node: FileExplorerMapNode): string {
  const parts: string[] = [];
  if (node.parse !== 'ok') parts.push(node.parse);
  for (const declaration of node.declarations) {
    const name = declaration.name ?? declaration.kind;
    parts.push(`${declaration.kind} ${name}:${declaration.line}`);
  }
  for (const purpose of node.purpose) parts.push(`doc:${purpose.line}`);
  return parts.join(' · ');
}

function edgeFilePath(edge: FileExplorerMapEdge, direction: 'outgoing' | 'incoming'): string | null {
  if (direction === 'incoming') return edge.source;
  return edge.target.type === 'file' ? edge.target.path : null;
}

function EdgeList({
  title,
  edges,
  direction,
  workspaceCwd,
  onOpenFile,
  testId,
}: {
  title: string;
  edges: FileExplorerMapEdge[];
  direction: 'outgoing' | 'incoming';
  workspaceCwd?: string;
  onOpenFile: (path: string) => void;
  testId: string;
}) {
  const files = edges.flatMap((edge) => {
    const path = edgeFilePath(edge, direction);
    return path == null ? [] : [{ edge, path }];
  });
  const external = edges.filter((edge) => edge.target.type === 'external');
  const unresolved = edges.filter((edge) => edge.target.type === 'unresolved');
  return (
    <section className="file-explorer__map-group" data-testid={testId}>
      <h3 className="file-explorer__map-heading">{title}</h3>
      {edges.length === 0 && <p className="file-explorer__muted">None in this scan.</p>}
      {files.map(({ edge, path }) => (
          <button
            key={`${edge.source}:${edge.line}:${path}`}
            type="button"
            className="file-explorer__map-file"
            onClick={() => onOpenFile(path)}
          >
            <span className="file-explorer__map-file-name">{safeText(fileExplorerDisplayPath(path, workspaceCwd))}</span>
            <span className="file-explorer__map-file-meta">{safeText(`${edge.kind} ${edge.specifier} :${edge.line}`)}</span>
          </button>
      ))}
      {external.length > 0 && (
        <ul className="file-explorer__map-external" aria-label="External">
          {external.map((edge) => (
            edge.target.type === 'external' ? (
              <li key={`${edge.source}:${edge.line}:${edge.target.specifier}`}>
                {safeText(`external ${edge.target.specifier} (${edge.kind} :${edge.line})`)}
              </li>
            ) : null
          ))}
        </ul>
      )}
      {unresolved.length > 0 && (
        <ul className="file-explorer__map-unresolved" aria-label="Unresolved">
          {unresolved.map((edge) => (
            edge.target.type === 'unresolved' ? (
              <li key={`${edge.source}:${edge.line}:${edge.target.reason}`}>
                {safeText(`unresolved ${edge.specifier} - ${edge.target.reason} :${edge.line}`)}
              </li>
            ) : null
          ))}
        </ul>
      )}
    </section>
  );
}

export function FileDependencyMap({ map, workspaceCwd, onOpenFile }: FileDependencyMapProps) {
  const diagram = fileExplorerMapDiagram(map);
  const markdown = '```mermaid\n' + diagram.source + '\n```';
  const coverage = map.coverage;
  const coverageNotes = [
    coverage.visibleEntriesOnly ? 'Visible workspace entries only; ignored files and symlinks are not a zero-dependent proof.' : '',
    coverage.incomplete ? 'Coverage is incomplete.' : '',
    coverage.truncated ? 'Scan truncated.' : '',
    coverage.unsupportedFiles > 0 ? `${coverage.unsupportedFiles} unsupported.` : '',
    coverage.unreadableFiles > 0 ? `${coverage.unreadableFiles} unreadable files.` : '',
    coverage.unreadableDirectories > 0 ? `${coverage.unreadableDirectories} unreadable directories.` : '',
    coverage.incompleteFiles > 0 ? `${coverage.incompleteFiles} incomplete parses.` : '',
    coverage.dynamicOmitted > 0 ? `${coverage.dynamicOmitted} dynamic imports omitted.` : '',
  ].filter((note) => note !== '');
  return (
    <div className="file-explorer__map" data-testid="file-dependency-map" data-map-path={map.path}>
      <MarkdownBody className="file-explorer__map-diagram" text={markdown} />
      <div className="file-explorer__map-nodes" data-testid="file-dependency-map-nodes">
        {map.nodes.map((node: FileExplorerMapNode) => {
          const selected = fileExplorerSamePath(node.path, map.path);
          const evidence = roleLine(node);
          return (
            <button
              key={node.path}
              type="button"
              data-map-node-path={node.path}
              className={`file-explorer__map-node${selected ? ' is-selected' : ''}`}
              aria-current={selected ? 'true' : undefined}
              onClick={() => onOpenFile(node.path)}
            >
              <span className="file-explorer__map-node-name">
                {safeText(fileExplorerBasename(node.path) || fileExplorerDisplayPath(node.path, workspaceCwd))}
              </span>
              <span className="file-explorer__map-node-path">{safeText(fileExplorerDisplayPath(node.path, workspaceCwd))}</span>
              {evidence !== '' && <span className="file-explorer__map-node-evidence">{safeText(evidence)}</span>}
              {node.purpose.map((purpose: { line: number; text: string }) => (
                <span key={`${node.path}:${purpose.line}`} className="file-explorer__map-node-doc">
                  {safeText(`:${purpose.line} ${purpose.text}`)}
                </span>
              ))}
            </button>
          );
        })}
      </div>
      <EdgeList title="Outgoing" edges={map.outgoing} direction="outgoing" workspaceCwd={workspaceCwd} onOpenFile={onOpenFile} testId="file-dependency-map-outgoing" />
      <EdgeList title="Incoming" edges={map.incoming} direction="incoming" workspaceCwd={workspaceCwd} onOpenFile={onOpenFile} testId="file-dependency-map-incoming" />
      {coverageNotes.length > 0 && (
        <p className="file-explorer__map-coverage" data-testid="file-dependency-map-coverage">
          {safeText(coverageNotes.join(' '))}
        </p>
      )}
    </div>
  );
}
