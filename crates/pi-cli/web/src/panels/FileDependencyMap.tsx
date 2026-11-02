// Directory dependency graph. One Mermaid fence rendered by the shared
// MarkdownBody. Node ids are opaque (`n0`...). Paths never enter click
// handlers or HTML.

import { MarkdownBody } from '../MarkdownBody';
import { safeText } from '../redact';
import {
  fileExplorerMapDiagram,
  type FileExplorerMapResult,
} from '../fileExplorer';

export interface FileDependencyMapProps {
  map: FileExplorerMapResult;
  workspaceCwd?: string;
  onOpenFile: (path: string) => void;
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
  return (
    <div className="file-explorer__map" data-testid="file-dependency-map" data-map-path={map.path}>
      <MarkdownBody className="file-explorer__map-diagram" text={markdown} />
      {coverageNotes.length > 0 && (
        <p className="file-explorer__map-coverage" data-testid="file-dependency-map-coverage">
          {safeText(coverageNotes.join(' '))}
        </p>
      )}
    </div>
  );
}
