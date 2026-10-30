#!/usr/bin/env node
// Pure selection-action behavior: painted-range authority, strict blame
// parse, and compact attribution labels. No panel mount, no RPC.
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import {
  fileExplorerBlameCommand,
  fileExplorerSamePath,
  normalizeBlame,
  type FileExplorerBlameLine,
  type FileExplorerBlameResult,
} from '../src/fileExplorer.ts';
import { fileExplorerAskSelectionRange, fileExplorerPaintedSelection, fileExplorerSelectionAnchor, FILE_EXPLORER_SELECTION_EDGE_PX } from '../src/panels/FileExplorerPanel.tsx';
import {
  FileSelectionActions,
  fileSelectionBlameDate,
  fileSelectionBlameMatches,
  fileSelectionCommitLabel,
  fileSelectionRangeLabel,
} from '../src/panels/FileSelectionActions.tsx';

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: unknown, detail = ''): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const file = { kind: 'file', startCursor: 10, lines: ['a', 'b', 'c'] };
const range = { startLine: 11, endLine: 12 };
const stored = { ownerKey: 'sess', path: 'src/a.ts', range };

check(
  'painted range accepts an inclusive window inside the page',
  JSON.stringify(fileExplorerPaintedSelection(stored, {
    ownerKey: 'sess', path: 'src/a.ts', view: 'file', file,
  })) === JSON.stringify(range),
);
check(
  'painted range rejects a line above the loaded window',
  fileExplorerPaintedSelection(
    { ...stored, range: { startLine: 10, endLine: 12 } },
    { ownerKey: 'sess', path: 'src/a.ts', view: 'file', file },
  ) === null,
);
check(
  'painted range rejects a line past the loaded window',
  fileExplorerPaintedSelection(
    { ...stored, range: { startLine: 12, endLine: 14 } },
    { ownerKey: 'sess', path: 'src/a.ts', view: 'file', file },
  ) === null,
);
check(
  'painted range rejects another owner, path, diff view, and a diff page',
  fileExplorerPaintedSelection(stored, { ownerKey: 'other', path: 'src/a.ts', view: 'file', file }) === null
    && fileExplorerPaintedSelection(stored, { ownerKey: 'sess', path: 'src/b.ts', view: 'file', file }) === null
    && fileExplorerPaintedSelection(stored, { ownerKey: 'sess', path: 'src/a.ts', view: 'diff', file }) === null
    && fileExplorerPaintedSelection(stored, {
      ownerKey: 'sess', path: 'src/a.ts', view: 'file', file: { ...file, kind: 'diff' },
    }) === null,
);
check('painted range rejects a missing selection', fileExplorerPaintedSelection(null, {
  ownerKey: 'sess', path: 'src/a.ts', view: 'file', file,
}) === null);
check(
  'ask range keeps a selection outside the painted window',
  JSON.stringify(fileExplorerAskSelectionRange(
    { ...stored, range: { startLine: 4, endLine: 7 } },
    { ownerKey: 'sess', path: 'src/a.ts', view: 'file' },
  )) === JSON.stringify({ startLine: 4, endLine: 7 }),
);
check(
  'ask range rejects another owner, path, and diff view',
  fileExplorerAskSelectionRange(stored, { ownerKey: 'other', path: 'src/a.ts', view: 'file' }) === null
    && fileExplorerAskSelectionRange(stored, { ownerKey: 'sess', path: 'src/b.ts', view: 'file' }) === null
    && fileExplorerAskSelectionRange(stored, { ownerKey: 'sess', path: 'src/a.ts', view: 'diff' }) === null
    && fileExplorerAskSelectionRange(null, { ownerKey: 'sess', path: 'src/a.ts', view: 'file' }) === null,
);

check(
  'blame command is path plus inclusive lines',
  JSON.stringify(fileExplorerBlameCommand('src/a.ts', 11, 12))
    === JSON.stringify({ type: 'file_explorer_blame', path: 'src/a.ts', startLine: 11, endLine: 12 }),
);

const oid = 'abcdef1234567890abcdef1234567890abcdef12';
function line(currentLine: number, commit: string | null, text: string): FileExplorerBlameLine {
  return { commit, author: 'Ada Lovelace', time: 1_712_345_678, originalLine: currentLine, currentLine, text };
}
const ok = normalizeBlame({
  path: 'src/a.ts',
  lines: [line(11, oid, 'const a = 1;'), line(12, null, 'const b = 2;')],
}, { path: 'src/a.ts', startLine: 11, endLine: 12 });
check('blame parse keeps ordered lines and a null commit', ok !== null
  && ok.lines.length === 2
  && ok.lines[0]?.commit === oid
  && ok.lines[1]?.commit === null
  && ok.lines[1]?.text === 'const b = 2;');
check('blame parse rejects a different path', normalizeBlame({
  path: 'src/b.ts', lines: [line(11, oid, 'a')],
}, { path: 'src/a.ts', startLine: 11, endLine: 11 }) === null);
check('blame parse rejects a gap and a reordered current line', normalizeBlame({
  path: 'src/a.ts', lines: [line(11, oid, 'a'), line(13, oid, 'c')],
}, { path: 'src/a.ts', startLine: 11, endLine: 12 }) === null);
check('blame parse rejects an email field', normalizeBlame({
  path: 'src/a.ts',
  lines: [{ ...line(11, oid, 'a'), email: 'ada@example.com' }],
}, { path: 'src/a.ts', startLine: 11, endLine: 11 }) === null);
check('blame parse rejects an empty commit and a non-string line', normalizeBlame({
  path: 'src/a.ts', lines: [{ ...line(11, oid, 'a'), commit: '' }],
}, { path: 'src/a.ts', startLine: 11, endLine: 11 }) === null
  && normalizeBlame({ path: 'src/a.ts', lines: 'nope' }, { path: 'src/a.ts', startLine: 11, endLine: 11 }) === null);
check('canonical path still matches the requested file', fileExplorerSamePath('./src/a.ts', 'src/a.ts') === true);

const parsed = ok as FileExplorerBlameResult;
check('stale blame for another range is rejected', fileSelectionBlameMatches(parsed, {
  path: 'src/a.ts', startLine: 11, endLine: 11,
}) === false);
check('exact blame range is accepted', fileSelectionBlameMatches(parsed, {
  path: 'src/a.ts', startLine: 11, endLine: 12,
}) === true);
check('range label is inclusive', fileSelectionRangeLabel({ startLine: 11, endLine: 11 }) === 'Line 11'
  && fileSelectionRangeLabel(range) === 'Lines 11–12');
check('null commit is Uncommitted and a full oid keeps its title', fileSelectionCommitLabel(null).label === 'Uncommitted'
  && fileSelectionCommitLabel(oid).label === oid.slice(0, 9)
  && fileSelectionCommitLabel(oid).title === oid);
const authorDate = new Date(1_712_345_678_000);
const expectedAuthorDate = [
  String(authorDate.getFullYear()),
  String(authorDate.getMonth() + 1).padStart(2, '0'),
  String(authorDate.getDate()).padStart(2, '0'),
].join('-');
check('author time formats as that local date', fileSelectionBlameDate(1_712_345_678) === expectedAuthorDate);
check('author time outside the Date range is rejected', fileSelectionBlameDate(8.64e12 + 1) === null
  && fileSelectionBlameDate(1.5) === null
  && normalizeBlame({
    path: 'src/a.ts', lines: [{ ...line(11, oid, 'a'), time: 8.64e12 + 1 }],
  }, { path: 'src/a.ts', startLine: 11, endLine: 11 }) === null);
check('blame parse rejects a short oid, the zero oid, and a fractional line', normalizeBlame({
  path: 'src/a.ts', lines: [{ ...line(11, oid, 'a'), commit: oid.slice(0, 9) }],
}, { path: 'src/a.ts', startLine: 11, endLine: 11 }) === null
  && normalizeBlame({
    path: 'src/a.ts', lines: [{ ...line(11, oid, 'a'), commit: '0'.repeat(40) }],
  }, { path: 'src/a.ts', startLine: 11, endLine: 11 }) === null
  && normalizeBlame({
    path: 'src/a.ts', lines: [{ ...line(11, oid, 'a'), originalLine: 11.5 }],
  }, { path: 'src/a.ts', startLine: 11, endLine: 11 }) === null);

const html = renderToStaticMarkup(createElement(FileSelectionActions, {
  range,
  pending: true,
  blame: parsed,
  onAsk: () => {},
  onBlame: () => {},
  onClose: () => {},
}));
check('toolbar sits in its own container with Ask and Blame', html.includes('data-testid="file-selection-actions"')
  && html.includes('role="toolbar"')
  && html.includes('aria-label="Ask"')
  && html.includes('aria-label="Blame"')
  && html.includes('>Ask<')
  && html.includes('>Blame<'));
check('pending disables both actions', html.includes('aria-busy="true"')
  && (html.match(/disabled=""/g) ?? []).length === 2);
check('null commit renders Uncommitted and the source line is text', html.includes('>Uncommitted<')
  && html.includes('>Ada Lovelace<')
  && html.includes('>const b = 2;<')
  && html.includes('data-line="12"'));
check('close is a quiet control', html.includes('aria-label="Close selection"') && html.includes('>\u00D7</button>'));
check('floating toolbar keeps Ask, Blame, and close reachable by name', html.includes('data-testid="file-selection-ask"')
  && html.includes('data-testid="file-selection-blame"')
  && html.includes('data-testid="file-selection-close"')
  && html.includes('data-testid="file-selection-blame-list"')
  && html.includes('aria-label="Close selection"'));
{
  const edge = FILE_EXPLORER_SELECTION_EDGE_PX;
  const viewport = { top: 0, left: 0, width: 360, height: 220 };
  const bar = { width: 160, height: 44 };
  const selection = { top: 48, left: 12, width: 200, height: 36 };
  const placed = fileExplorerSelectionAnchor({ viewport, selection, bar, edge });
  const selectionBottom = selection.top + selection.height;
  const overlaps = placed.top < selectionBottom && placed.top + bar.height > selection.top
    && placed.left < selection.left + selection.width && placed.left + bar.width > selection.left;
  check(
    'narrow 360 anchor stays inside 8px and clear of the selection',
    placed.visible === true
      && placed.left >= edge
      && placed.left + bar.width <= viewport.width - edge
      && placed.top >= edge
      && placed.top + bar.height <= viewport.height - edge
      && overlaps === false,
  );
}

console.log(`\nFileSelectionActions.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  for (const failure of failures) console.error(`  FAIL ${failure}`);
  process.exit(1);
}
process.exit(0);
