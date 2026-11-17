#!/usr/bin/env node
// Files default layout, before the production change.
//
// A fresh view store uses round(34 * 0.85) = 29. A stored tree percent stays.
// The rendered panel header has no Expand/Shrink control even when an expand
// callback is supplied. Geometry stays in the browser lane.
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import { FileExplorerPanel } from '../src/panels/FileExplorerPanel.tsx';
import {
  emptyFileExplorerSlot,
  emptyFileExplorerViewState,
  fileExplorerViewForOwner,
  patchFileExplorerViewCache,
  readModuleFileExplorerViewCache,
  writeModuleFileExplorerViewCache,
} from '../src/fileExplorer.ts';

const EXPECTED_FRESH_TREE_PCT = Math.round(34 * 0.85);
const failures: string[] = [];
let ran = 0;

function check(name: string, condition: unknown, detail = ''): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

writeModuleFileExplorerViewCache({});
const fresh = fileExplorerViewForOwner(readModuleFileExplorerViewCache(), 'sess-layout-fresh');
check(
  'fresh view tree percent is 29',
  fresh.treePct === EXPECTED_FRESH_TREE_PCT && emptyFileExplorerViewState().treePct === EXPECTED_FRESH_TREE_PCT,
  String(fresh.treePct),
);
patchFileExplorerViewCache('sess-layout-custom', { treePct: 41 });
check(
  'stored tree percent stays 41',
  fileExplorerViewForOwner(readModuleFileExplorerViewCache(), 'sess-layout-custom').treePct === 41,
);
check(
  'stored owner does not change the fresh default',
  fileExplorerViewForOwner(readModuleFileExplorerViewCache(), 'sess-layout-other').treePct === EXPECTED_FRESH_TREE_PCT,
);

const html = renderToStaticMarkup(createElement(FileExplorerPanel, {
  sendCommand: () => Promise.resolve({}),
  hostAuthority: '127.0.0.1:1',
  pageProtocol: 'http:',
  connectionEpoch: 0,
  onPatchSlot: () => {},
  onConversationError: () => {},
  onOpenPath: () => {},
  onClose: () => {},
  sessionId: 'sess-layout-panel',
  slot: emptyFileExplorerSlot(),
}));
const headerStart = html.indexOf('data-testid="file-explorer-header"');
const bodyStart = html.indexOf('data-testid="file-explorer-layout"');
const header = headerStart >= 0 && bodyStart > headerStart ? html.slice(headerStart, bodyStart) : '';
check('panel header has no expand control', !header.includes('expand-btn') && !header.includes('Expand file explorer') && !header.includes('Shrink file explorer'));
check('panel keeps the tree and close control', header.includes('file-explorer-drawer-close-btn') && html.includes('data-testid="file-explorer-tree"'));

console.log(`\nfileExplorerDefaultLayout.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  for (const failure of failures) console.error(`  FAIL ${failure}`);
  process.exit(1);
}
