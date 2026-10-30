#!/usr/bin/env node
// File-explorer RPC wire + drawer session-state regression for
// src/fileExplorer.ts. Proves frozen-contract command builders, defensive
// normalizers (empty changes / non-repo / binary-shaped errors stay
// path-free), paging merge, path-focus dedup (same path focuses existing),
// close/reopen preserving selection, and A→B session rebinding. Also
// renders FileExplorerPanel through react-dom/server for two-viewport Files
// structure (not geometry). Run via `npm run build`.
import { createElement } from 'react';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import {
  ChangeStatusButton,
  FileExplorerPanel,
  FILE_EXPLORER_TOGGLE_ID,
  FILE_EXPLORER_TREE_PCT_DEFAULT,
  FILE_EXPLORER_TREE_PCT_MAX,
  FILE_EXPLORER_TREE_PCT_MIN,
  FILE_EXPLORER_TREE_PCT_STEP,
  clampFileExplorerTreePct,
  fileExplorerContentPaneKind,
  fileExplorerControlId,
  fileExplorerHandleDocumentKeyDown,
  fileExplorerKeydownScope,
  fileExplorerTreePctForKey,
  fileExplorerTreePctFromPointer,
  fileExplorerDownloadSettledError,
  stepFileExplorerTreePct,
} from '../src/panels/FileExplorerPanel.tsx';
import { markdownOpenRejectionOwnership } from '../src/transportError.ts';
import {
  MARKDOWN_OPEN_MISSING_PATH_MAX_CHARS,
  markdownOpenBoundedPathText,
  markdownOpenErrorDetail,
  markdownOpenMissingFileVerdict,
  markdownOpenMissingFileToast,
} from '../src/App.tsx';

import { FileTypeIcon } from '../src/components/FileTypeIcon.tsx';

import {
  FILE_EXPLORER_DEFAULT_MAX_LINES,
  FILE_EXPLORER_HARD_MAX_LINES,
  FILE_EXPLORER_MAX_CACHE_BYTES,
  FILE_EXPLORER_MAX_CACHE_ENTRIES,
  FILE_EXPLORER_MAX_CACHE_LINES,
  FILE_EXPLORER_MAX_OPEN_TABS,
  FILE_EXPLORER_MAX_SESSIONS,
  FILE_EXPLORER_MAX_LINES_PER_ENTRY,
  FILE_EXPLORER_NEXT_PAGE_NEAR_BOTTOM_PX,
  FILE_EXPLORER_MAX_PREVIEW_BYTES,
  FILE_EXPLORER_TOO_LARGE_MESSAGE,
  FILE_EXPLORER_SIZE_UNAVAILABLE_MESSAGE,
  fileExplorerResolvedRequestPath,
  fileExplorerResponseMatchesRequest,
  applyCloseTab,
  applyDrawerMode,
  applyOpenPath,
  applySelectTab,
  appendPagedLines,
  clampMaxLines,
  emptyFileExplorerPageCache,
  emptyFileExplorerSlot,
  emptyFileExplorerViewState,
  fileExplorerApplyCachedPage,
  fileExplorerApplyPage,
  fileExplorerBasename,
  fileExplorerBeginCachedPage,
  fileExplorerBeginPage,
  fileExplorerBoundPageCache,
  fileExplorerApplyViewPatch,
  fileExplorerBuildChangeIndex,
  fileExplorerSyncViewCacheWithSlots,
  fileExplorerToggleExpandedDir,
  fileExplorerViewForOwner,
  getFileExplorerViewCacheSnapshot,
  patchFileExplorerViewCache,
  readModuleFileExplorerViewCache,
  subscribeFileExplorerViewCache,
  updateFileExplorerDirCache,
  updateFileExplorerViewCache,
  fileExplorerWriteDirState,
  writeModuleFileExplorerViewCache,
  fileExplorerCanLoadMore,
  fileExplorerCanonicalPath,
  fileExplorerCapSessionTabs,
  fileExplorerChangesCommand,
  fileExplorerContentSurface,
  fileExplorerIsMarkdownPath,
  fileExplorerConversationDraft,
  fileExplorerPreviewKind,
  fileExplorerWithConversationDraft,
  fileExplorerWithPathSurface,
  fileExplorerAcquireInflight,
  fileExplorerAcquireDirInflight,
  fileExplorerPeekInflight,
  fileExplorerReleaseInflight,
  fileExplorerSettleInflightCache,
  fileExplorerDiffCommand,
  fileExplorerDirAggregate,
  fileExplorerDisplayPath,
  fileExplorerDownloadFileName,
  fileExplorerFailCachedPage,
  fileExplorerFailPage,
  fileExplorerFailDirState,
  fileExplorerBeginDirState,
  fileExplorerApplyDirState,
  fileExplorerEmptyDirState,
  fileExplorerInflightKey,
  nextFileExplorerRequestGeneration,
  fileExplorerPeekRequestGeneration,
  fileExplorerPeekDirInflight,
  fileExplorerListCommand,
  fileExplorerLoadedEnd,
  fileExplorerMergeTree,
  fileExplorerDiffStateAfter,
  fileExplorerIsOversized,
  fileExplorerPageCacheEstimate,
  fileExplorerPageCacheKey,
  fileExplorerReadCachedPage,
  fileExplorerReadCommand,
  fileExplorerSanitizePageCache,
  fileExplorerSamePath,
  fileExplorerSetCachedScrollTop,
  fileExplorerShouldFetchPage,
  fileExplorerShouldLoadNext,
  fileExplorerShouldRetryTransportDirs,
  fileExplorerShouldRetryTransportPage,
  fileExplorerRetryableTransportDirPaths,
  fileExplorerStageDownloadCommand,
  fileExplorerSyncPageCacheWithSlots,
  fileExplorerTransportIncompleteDirPaths,
  fileExplorerTransportRetryCursor,
  fileExplorerTruncationInconsistent,
  fileExplorerCommandForSession,
  fileExplorerCursorForLine,
  fileExplorerShouldRestoreFileScroll,
  fileExplorerLoadedDescendantMatches,
  fileExplorerNormalizeTreeQuery,
  fileExplorerTreeQueryMatches,
  fileExplorerTreeTone,
  normalizeChanges,
  normalizeDiff,
  normalizeList,
  normalizeRead,
  normalizeStageDownload,
  normalizeFileConversationSnapshot,
  normalizeFileConversationClose,
  fileConversationCloseAckValid,
  fileConversationPromptAccepted,
  closeFileConversationsDroppedBySlotPatch,
  parseFileExplorerUnifiedDiff,
  groupFileExplorerDiffRows,
  fileExplorerStatusLetter,
  openFileTab,
  readFileExplorerSlot,
  readModuleFileExplorerPageCache,
  triggerFileExplorerDownload,
  updateFileExplorerSlot,
  writeModuleFileExplorerPageCache,
  type FileExplorerPageCache,
  type FileExplorerPageError,
  type FileExplorerPageState,
} from '../src/fileExplorer.ts';

const failures: string[] = [];
const pending: Promise<unknown>[] = [];
let ran = 0;
function check(name: string, cond: unknown, detail?: string): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function slotOf(partial: Partial<FileExplorerSessionSlot>): FileExplorerSessionSlot {
  return { tabs: [], activePath: null, mode: 'closed', ...partial };
}

function countToken(haystack: string, token: string): number {
  return haystack.split(token).length - 1;
}

function cssMinHeightAtLeast(css: string, selector: string, minPx: number): boolean {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const re = new RegExp(`${escaped}[^{]*\\{[^}]*min-height\\s*:\\s*(\\d+)px`, 'g');
  let matched = false;
  let m: RegExpExecArray | null;
  while ((m = re.exec(css))) {
    matched = true;
    if (Number(m[1]) < minPx) return false;
  }
  return matched;
}

function cssDeclaresMinBox(css: string, selector: string, minPx: number): boolean {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const re = new RegExp(`${escaped}[^{]*\\{([^}]*)}`, 'g');
  let m: RegExpExecArray | null;
  while ((m = re.exec(css))) {
    const body = m[1];
    const height = body.match(/min-height\s*:\s*(\d+)px/);
    const width = body.match(/min-width\s*:\s*(\d+)px/);
    if (height && Number(height[1]) >= minPx && width && Number(width[1]) >= minPx) return true;
  }
  return false;
}

function obsoleteExplorerChrome(html: string): string[] {
  const hits: string[] = [];
  for (const token of [
    'FILE TABS',
    'file-explorer__content-head',
    'file-explorer__footer',
    'Changed files',
    'No open files',
    'File view',
    'file-explorer__tabs',
    'file-explorer__tab',
    'file-explorer__section',
    'file-explorer__list',
    'role="tablist"',
    'aria-label="Open files"',
    'aria-label="Changed files"',
    'aria-label="File view"',
    'file-explorer__download',
    '>Download</button>',
    'file-explorer__more',
    '>Load more</button>',
    'file-explorer-close-btn',
    'file-explorer__content-path',
    'data-orphans="true"',
  ]) {
    if (html.includes(token)) hits.push(token);
  }
  if (/(?:role="tab"|role='tab')(?!list)/.test(html)) hits.push('role="tab"');
  if (/>Workspace</.test(html)) hits.push('Workspace section');
  return hits;
}

function assertTwoViewportStructure(label: string, html: string): void {
  const headerOpen = html.indexOf('<div class="file-explorer__head"');
  const layoutStart = html.indexOf('data-testid="file-explorer-layout"');
  const head = headerOpen >= 0 && layoutStart > headerOpen ? html.slice(headerOpen, layoutStart) : '';
  const actionsOpen = head.indexOf('file-explorer__actions');
  const title = actionsOpen >= 0 ? head.slice(0, actionsOpen) : head;
  const refreshAt = head.indexOf('file-explorer-refresh-btn');
  const closeAt = head.indexOf('file-explorer-drawer-close-btn');
  const layout = layoutStart >= 0 ? html.slice(layoutStart) : '';
  const treeColumn = layout.indexOf('data-testid="file-explorer-tree"');
  const treeList = layout.indexOf('class="file-explorer__tree-list"');
  const contentToolbar = layout.indexOf('file-explorer__content-toolbar');
  check(`${label}: Files title is static text`, title.includes('>Files<') && !/>Files<\/button>/.test(title));
  check(`${label}: header text is Files`, html.includes('>Files<') && !html.includes('FILE TABS'));
  check(`${label}: header testid`, html.includes('data-testid="file-explorer-header"'));
  check(
    `${label}: drawer close is the sole close`,
    countToken(html, 'id="file-explorer-drawer-close-btn"') === 1
      && html.includes('aria-label="Close file explorer"')
      && html.includes('title="Close file explorer"')
      && !html.includes('file-explorer-hide-btn')
      && !html.includes('Hide file explorer'),
  );
  check(`${label}: no separate hide button or » glyph`, !html.includes('file-explorer__hide') && !html.includes('»'));
  check(
    `${label}: search is the tree column toolbar and precedes the only toggle`,
    !html.includes('file-explorer__tree-edge')
      && treeColumn >= 0
      && treeColumn < layout.indexOf('file-explorer-search')
      && layout.indexOf('file-explorer-search') < layout.indexOf('id="file-explorer-tree-toggle"')
      && layout.includes('aria-label="Collapse file tree"')
      && treeList > layout.indexOf('file-explorer-search')
      && (html.match(/id="file-explorer-tree-toggle"/g) ?? []).length === 1,
  );
  check(
    `${label}: icon-only Refresh sits in actions before close`,
    head.includes('file-explorer__actions') && refreshAt > head.indexOf('file-explorer__actions') && closeAt > refreshAt && head.includes('aria-label="Refresh"') && head.includes('file-explorer__refresh-glyph') && !head.includes('file-explorer__refresh-label'),
  );
  const obsolete = obsoleteExplorerChrome(html);
  check(`${label}: rejected chrome absent`, obsolete.length === 0, obsolete.join(','));
  check(
    `${label}: layout wrap`,
    html.includes('data-testid="file-explorer-layout"')
      && html.includes('data-layout="split"')
      && html.includes('file-explorer__body'),
  );
  check(
    `${label}: tree viewport`,
    html.includes('data-testid="file-explorer-tree"')
      && html.includes('aria-label="Workspace files"')
      && html.includes('file-explorer__tree-viewport')
      && html.includes('role="region"'),
  );
  check(
    `${label}: content viewport`,
    html.includes('data-testid="file-explorer-content"')
      && html.includes('aria-label="Selected file"')
      && html.includes('file-explorer__content-viewport')
      && contentToolbar > layoutStart,
  );
  check(
    `${label}: divider`,
    html.includes('data-testid="file-explorer-divider"')
      && html.includes('file-explorer__divider')
      && html.includes('role="separator"')
      && html.includes('aria-orientation="vertical"')
      && html.includes('aria-valuemin="22"')
      && html.includes('aria-valuemax="55"')
      && html.includes('aria-valuenow="34"')
      && html.includes('aria-label="Resize file tree"')
      && html.includes('tabindex="0"'),
  );
  check(`${label}: tree viewport once`, countToken(html, 'data-testid="file-explorer-tree"') === 1);
  check(`${label}: content viewport once`, countToken(html, 'data-testid="file-explorer-content"') === 1);
  check(`${label}: layout wrap once`, countToken(html, 'data-testid="file-explorer-layout"') === 1);
}

// ---- command builders: snake_case type, camelCase fields ----
{
  check('list root omits path', JSON.stringify(fileExplorerListCommand()) === JSON.stringify({ type: 'file_explorer_list' }));
  check('list empty path omits path', JSON.stringify(fileExplorerListCommand('')) === JSON.stringify({ type: 'file_explorer_list' }));
  check('list relative path is camelCase', JSON.stringify(fileExplorerListCommand('src')) === JSON.stringify({ type: 'file_explorer_list', path: 'src' }));
  check('read default has path only', JSON.stringify(fileExplorerReadCommand('a.ts')) === JSON.stringify({ type: 'file_explorer_read', path: 'a.ts' }));
  check(
    'read paging fields camelCase',
    JSON.stringify(fileExplorerReadCommand('a.ts', 40, 400)) === JSON.stringify({ type: 'file_explorer_read', path: 'a.ts', cursor: 40, maxLines: 400 }),
  );
  check('changes has no extra fields', JSON.stringify(fileExplorerChangesCommand()) === JSON.stringify({ type: 'file_explorer_changes' }));
  check(
    'diff paging fields camelCase',
    JSON.stringify(fileExplorerDiffCommand('a.ts', 0, 400)) === JSON.stringify({ type: 'file_explorer_diff', path: 'a.ts', cursor: 0, maxLines: 400 }),
  );
  check(
    'stage_download path only',
    JSON.stringify(fileExplorerStageDownloadCommand('bin.dat')) === JSON.stringify({ type: 'file_explorer_stage_download', path: 'bin.dat' }),
  );
}

{
  check(
    'session owner is explicit on file requests',
    JSON.stringify(fileExplorerCommandForSession('sess-a', fileExplorerReadCommand('src/a.ts', 400, 400)))
      === JSON.stringify({ type: 'file_explorer_read', path: 'src/a.ts', cursor: 400, maxLines: 400, sessionId: 'sess-a' }),
  );
  check('line 450 starts at cursor 400', fileExplorerCursorForLine(450) === 400);
  check('first page line omits cursor', fileExplorerCursorForLine(400) === undefined);
  check('line 801 starts at cursor 800', fileExplorerCursorForLine(801) === 800);
  check('line 299 is on the first page', fileExplorerCursorForLine(299) === undefined);
  check('a target line blocks content scroll restore', fileExplorerShouldRestoreFileScroll(299) === false);
  check('no target line restores content scroll', fileExplorerShouldRestoreFileScroll(undefined) === true);
  const openedAtLine = applyOpenPath(emptyFileExplorerSlot(), 'file.rs', 299);
  check(
    'opening file.rs:299 stores the target line',
    openedAtLine.activePath === 'file.rs' && openedAtLine.tabs[0]?.line === 299,
    JSON.stringify(openedAtLine.tabs),
  );
}

// ---- maxLines clamp ----
{
  check('default maxLines is 400', FILE_EXPLORER_DEFAULT_MAX_LINES === 400);
  check('hard max is 2000', FILE_EXPLORER_HARD_MAX_LINES === 2000);
  check('clamp NaN -> default', clampMaxLines(Number.NaN) === 400);
  check('clamp 0 -> default', clampMaxLines(0) === 400);
  check('clamp 99999 -> 2000', clampMaxLines(99999) === 2000);
  const clamped = fileExplorerReadCommand('a.ts', undefined, 5000);
  check('read command clamps maxLines', typeof clamped.maxLines === 'number' && clamped.maxLines === 2000);
}

// ---- normalizers: empty / non-repo / malformed fail closed ----
{
  check('list null -> null', normalizeList(null) === null);
  check('list missing entries -> null', normalizeList({}) === null);
  const emptyList = normalizeList({ entries: [], truncated: false });
  check('list empty entries ok', emptyList !== null && emptyList.entries.length === 0 && emptyList.truncated === false);
  const listed = normalizeList({
    entries: [
      { name: 'src', path: 'src', kind: 'dir' },
      { name: 'a.ts', path: 'src/a.ts', kind: 'file', sizeBytes: 12 },
      { name: 'bad', path: 'x', kind: 'symlink' },
      null,
    ],
    truncated: true,
  });
  check('list drops malformed kinds', listed !== null && listed.entries.length === 2 && listed.truncated === true);
  check('list file sizeBytes kept', listed !== null && listed.entries[1].sizeBytes === 12);

  const emptyChanges = normalizeChanges({ files: [], isRepo: false });
  check('non-repo is empty files + isRepo false', emptyChanges !== null && emptyChanges.files.length === 0 && emptyChanges.isRepo === false);
  const changes = normalizeChanges({
    files: [
      { path: 'a.ts', status: 'modified' },
      { path: 'b.ts', status: 'renamed', oldPath: 'c.ts' },
      { path: 'x', status: 'copied' },
    ],
    isRepo: true,
  });
  check('changes drops unknown status', changes !== null && changes.files.length === 2 && changes.files[1].oldPath === 'c.ts');

  check('read missing path -> null', normalizeRead({ lines: [] }) === null);
  const page = normalizeRead({ path: 'a.ts', lines: ['one', 2, 'three'], sizeBytes: 9, truncated: true, nextCursor: 2, totalLines: 491 });
  check('read drops non-string lines', page !== null && page.lines.length === 2 && page.nextCursor === 2 && page.truncated === true);
  check('read keeps backend totalLines', page !== null && page.totalLines === 491);
  const last = normalizeRead({ path: 'a.ts', lines: ['end'], sizeBytes: 3, truncated: false });
  check('read final page has no nextCursor', last !== null && last.nextCursor === undefined);
  check('read missing totalLines floors to page length', last !== null && last.totalLines === 1);

  const diff = normalizeDiff({ path: 'a.ts', lines: ['+x'], truncated: false, totalLines: 3 });
  check('diff ok', diff !== null && diff.lines[0] === '+x' && diff.nextCursor === undefined && diff.totalLines === 3);
  check('stage download requires both fields', normalizeStageDownload({ downloadId: 'x' }) === null);
  check(
    'stage download ok',
    JSON.stringify(normalizeStageDownload({ downloadId: '11111111-1111-1111-1111-111111111111', fileName: 'a.ts' }))
      === JSON.stringify({ downloadId: '11111111-1111-1111-1111-111111111111', fileName: 'a.ts' }),
  );
}

// ---- paging merge ----
{
  const first = { path: 'a.ts', lines: ['a', 'b'], nextCursor: 2, truncated: true, sizeBytes: 4 };
  const second = { path: 'a.ts', lines: ['c'], truncated: false };
  const merged = appendPagedLines(first, second);
  check('append concatenates lines', merged.lines.join(',') === 'a,b,c');
  check('append drops nextCursor on final page', merged.nextCursor === undefined);
  check('append copies truncated from page', merged.truncated === false);
}

// ---- canonical path identity: './x' tab vs backend 'x' ----
{
  check('canonical strips ./', fileExplorerCanonicalPath('./x') === 'x');
  check('canonical strips repeated slashes', fileExplorerCanonicalPath('a//b/') === 'a/b');
  check('canonical resolves interior ..', fileExplorerCanonicalPath('a/b/../c') === 'a/c');
  check('canonical keeps leading ..', fileExplorerCanonicalPath('../x') === '../x');
  check('canonical keeps absolute', fileExplorerCanonicalPath('/a/./b') === '/a/b');
  check('canonical empty stays empty', fileExplorerCanonicalPath('   ') === '');
  check('same path ./x vs x', fileExplorerSamePath('./x', 'x') === true);
  check('same path a//b vs a/b', fileExplorerSamePath('a//b', 'a/b') === true);
  check('different paths differ', fileExplorerSamePath('a.ts', 'b.ts') === false);
}

// ---- page-request ownership: A response after B selection ----
{
  const reqA = { sessionId: 'sess-a', path: 'a.ts', view: 'file' as const, generation: 1 };
  const reqB = { sessionId: 'sess-a', path: 'b.ts', view: 'file' as const, generation: 2 };
  const stateA = fileExplorerBeginPage(null, reqA);
  check('begin A is loading', stateA.loading === true && stateA.path === 'a.ts' && stateA.generation === 1);
  const stateB = fileExplorerBeginPage(stateA, reqB);
  check('begin B resets lines', stateB.loading === true && stateB.lines.length === 0 && stateB.path === 'b.ts');
  const appliedA = fileExplorerApplyPage(stateB, reqA, { path: 'a.ts', lines: ['late A'], truncated: false });
  check('stale A response cannot overwrite B', appliedA === stateB && appliedA.loading === true);
  const appliedB = fileExplorerApplyPage(stateB, reqB, { path: 'b.ts', lines: ['B content'], truncated: false });
  check('current B response applies', appliedB !== null && appliedB.lines.join(',') === 'B content' && appliedB.loading === false);
  check('fresh response keeps request ownership', appliedB !== null && appliedB.generation === 2 && appliedB.sessionId === 'sess-a');
  const failedA = fileExplorerFailPage(appliedB, reqA, { message: 'boom' });
  check('stale A failure cannot settle B', failedA === appliedB && appliedB.loading === false);
  const codedB = fileExplorerFailPage(appliedB, reqB, { message: 'arbitrary prose', code: 'file_line_too_large' });
  const staleOverCoded = fileExplorerFailPage(codedB, reqA, { message: 'stale overwrite', code: 'file_unavailable' });
  check(
    'stale A failure cannot replace B structured error',
    staleOverCoded === codedB
      && codedB?.error?.message === 'arbitrary prose'
      && codedB?.error?.code === 'file_line_too_large',
  );
}

// ---- content-pane kind: a selected file never yields a blank viewport ----
// fileExplorerContentPaneKind is the single mutually-exclusive status
// source for the content pane. Every combination of selection and page
// state maps to exactly one of hint / loading / error / empty / ready.
{
  check('no path is a hint', fileExplorerContentPaneKind(null, null) === 'hint');
  check('no path is a hint even with a page', fileExplorerContentPaneKind(null, { loading: false, error: null, lines: ['x'] }) === 'hint');
  check('path with no page state is loading', fileExplorerContentPaneKind('a.ts', null) === 'loading');
  check('path with in-flight first page is loading', fileExplorerContentPaneKind('a.ts', { loading: true, error: null, lines: [] }) === 'loading');
  check('path with in-flight reload keeps prior lines ready', fileExplorerContentPaneKind('a.ts', { loading: true, error: null, lines: ['keep'] }) === 'ready');
  check('settled error is error', fileExplorerContentPaneKind('a.ts', { loading: false, error: { message: 'read failed' }, lines: [] }) === 'error');
  check('error with prior content is still error', fileExplorerContentPaneKind('a.ts', { loading: false, error: { message: 'reload failed' }, lines: ['keep'] }) === 'error');
  check('settled zero lines is empty', fileExplorerContentPaneKind('a.ts', { loading: false, error: null, lines: [] }) === 'empty');
  check(
    'transport-incomplete with no lines is retryable error not empty',
    fileExplorerContentPaneKind('a.ts', { loading: false, error: null, lines: [], transportIncomplete: true }) === 'error',
  );
  check(
    'transport-incomplete with prior lines stays ready',
    fileExplorerContentPaneKind('a.ts', { loading: false, error: null, lines: ['keep'], transportIncomplete: true }) === 'ready',
  );
}

// ---- out-of-order cached apply: A response after B begin ----
// The cache is keyed per session/path/view, so a late A response must
// never touch B's entry: B stays in-flight until its own response lands,
// and the stale A apply is generation-rejected on A's own key.
{
  const sessionId = 'sess-ooo';
  const reqA = { sessionId, path: 'a.ts', view: 'file' as const, generation: 1 };
  const reqB = { sessionId, path: 'b.ts', view: 'file' as const, generation: 2 };
  let cache = emptyFileExplorerPageCache();
  cache = fileExplorerBeginCachedPage(cache, reqA);
  cache = fileExplorerBeginCachedPage(cache, reqB);
  const bInflight = fileExplorerReadCachedPage(cache, sessionId, 'b.ts', 'file');
  check('B begins in-flight', bInflight !== null && bInflight.loading === true && bInflight.lines.length === 0 && bInflight.path === 'b.ts');

  // A's response arrives after B began: it must not clobber B's entry.
  cache = fileExplorerApplyCachedPage(cache, reqA, { path: 'a.ts', lines: ['late A'], truncated: false });
  const bAfterStaleA = fileExplorerReadCachedPage(cache, sessionId, 'b.ts', 'file');
  check('stale A apply leaves B in-flight', bAfterStaleA !== null && bAfterStaleA.loading === true && bAfterStaleA.lines.length === 0 && bAfterStaleA.path === 'b.ts');
  const aAfterStaleA = fileExplorerReadCachedPage(cache, sessionId, 'a.ts', 'file');
  check('stale A apply settles A with its own content', aAfterStaleA !== null && aAfterStaleA.loading === false && aAfterStaleA.lines.join(',') === 'late A');

  // B's own response settles B independently.
  cache = fileExplorerApplyCachedPage(cache, reqB, { path: 'b.ts', lines: ['B content'], truncated: false });
  const bSettled = fileExplorerReadCachedPage(cache, sessionId, 'b.ts', 'file');
  check('B response settles B', bSettled !== null && bSettled.loading === false && bSettled.lines.join(',') === 'B content' && bSettled.generation === 2);
  const aUntouched = fileExplorerReadCachedPage(cache, sessionId, 'a.ts', 'file');
  check('B settle leaves A untouched', aUntouched !== null && aUntouched.lines.join(',') === 'late A' && aUntouched.generation === 1);

  // A newer same-key request supersedes the settled A page; the old
  // generation-1 response must not clobber it (generation-rejected).
  const reloadA = { ...reqA, generation: 3 };
  cache = fileExplorerBeginCachedPage(cache, reloadA);
  cache = fileExplorerApplyCachedPage(cache, reqA, { path: 'a.ts', lines: ['stale'], truncated: false });
  const aAfterStaleReload = fileExplorerReadCachedPage(cache, sessionId, 'a.ts', 'file');
  check('stale A apply after A reload is generation-rejected', aAfterStaleReload !== null && aAfterStaleReload.loading === true && aAfterStaleReload.lines.join(',') === 'late A');
  cache = fileExplorerApplyCachedPage(cache, reloadA, { path: 'a.ts', lines: ['fresh A'], truncated: false });
  const aSettled = fileExplorerReadCachedPage(cache, sessionId, 'a.ts', 'file');
  check('fresh A response settles the reload', aSettled !== null && aSettled.loading === false && aSettled.lines.join(',') === 'fresh A' && aSettled.generation === 3);
  const bStill = fileExplorerReadCachedPage(cache, sessionId, 'b.ts', 'file');
  check('A reload leaves settled B intact', bStill !== null && bStill.lines.join(',') === 'B content' && bStill.loading === false);
}

// ---- failed same-tab read keeps content and settles loading ----
{
  const req = { sessionId: 'sess-a', path: 'a.ts', view: 'file' as const, generation: 1 };
  const loaded = fileExplorerApplyPage(fileExplorerBeginPage(null, req), req, {
    path: 'a.ts', lines: ['keep me'], truncated: false,
  });
  const reload = fileExplorerBeginPage(loaded, { ...req, generation: 2 });
  check('same-tab reload keeps prior lines', reload.lines.join(',') === 'keep me' && reload.loading === true);
  const failed = fileExplorerFailPage(reload, { ...req, generation: 2 }, { message: 'read failed' });
  check('failed same-tab read keeps content', failed !== null && failed.lines.join(',') === 'keep me');
  check('failed same-tab read settles loading', failed !== null && failed.loading === false && failed.error?.message === 'read failed' && failed.error?.code === undefined);
}

// ---- transport failure settles loading without a local message ----
{
  const req = { sessionId: 'sess-a', path: 'a.ts', view: 'file' as const, generation: 1 };
  const loaded = fileExplorerApplyPage(fileExplorerBeginPage(null, req), req, {
    path: 'a.ts', lines: ['visible'], truncated: false,
  });
  const reload = fileExplorerBeginPage(loaded, { ...req, generation: 2 });
  const settled = fileExplorerFailPage(reload, { ...req, generation: 2 }, null);
  check('transport failure settles loading', settled !== null && settled.loading === false);
  check('transport failure keeps prior content', settled !== null && settled.lines.join(',') === 'visible');
  check('transport failure has no local error', settled !== null && settled.error === null);
  check('transport failure latches transportIncomplete', settled !== null && settled.transportIncomplete === true);
  check('begin clears transportIncomplete', reload.transportIncomplete === false);
}

// ---- './x' canonical response + load-more append ----
{
  const req = { sessionId: 'sess-a', path: './x', view: 'file' as const, generation: 1 };
  const first = fileExplorerApplyPage(fileExplorerBeginPage(null, req), req, {
    path: 'x', lines: ['one'], nextCursor: 1, truncated: true, sizeBytes: 4,
  });
  check('canonical response accepted for ./x tab', first !== null && first.lines.join(',') === 'one' && first.nextCursor === 1);
  check('tab identity stays user-owned', first !== null && first.path === './x');
  const more = fileExplorerBeginPage(first, { ...req, cursor: 1, generation: 2 });
  check('load-more keeps first page', more.lines.join(',') === 'one');
  const second = fileExplorerApplyPage(more, { ...req, cursor: 1, generation: 2 }, {
    path: 'x', lines: ['two'], truncated: false, sizeBytes: 4,
  });
  check('load-more appends canonical page', second !== null && second.lines.join(',') === 'one,two');
  check('load-more settles loading', second !== null && second.loading === false && second.nextCursor === undefined);
}

// ---- cross-layer identity: absolute request, relative wire response ----
// Backend always returns the scoped workspace-relative path. Absolute
// request identity stays request-local; display privacy never renders raw.
{
  const req = { sessionId: 'sess-a', path: '/workspace/notes.txt', view: 'file' as const, generation: 1 };
  const state = fileExplorerBeginPage(null, req);
  const applied = fileExplorerApplyPage(state, req, {
    path: 'notes.txt', lines: ['relative wire'], truncated: false, totalLines: 1,
  });
  check('absolute request accepts relative wire path', applied !== null && applied.lines.join(',') === 'relative wire' && applied.loading === false);
  check('relative wire keeps request-local tab identity', applied !== null && applied.path === '/workspace/notes.txt');
  check('relative wire keeps request ownership', applied !== null && applied.generation === 1 && applied.sessionId === 'sess-a');
  const messy = fileExplorerApplyPage(state, req, {
    path: 'src/../notes.txt', lines: ['messy relative'], truncated: false, totalLines: 1,
  });
  check('messy relative echo collapses to request identity', messy !== null && messy.lines.join(',') === 'messy relative');
  const wrongRel = fileExplorerApplyPage(state, req, {
    path: 'other.txt', lines: ['wrong'], truncated: false, totalLines: 1,
  });
  check('unrelated relative path still rejected', wrongRel !== null && wrongRel.error?.message === 'response path mismatch' && wrongRel.error?.code === undefined);
  const relativeReq = { sessionId: 'sess-a', path: 'src/a.ts', view: 'file' as const, generation: 1 };
  const relativeApplied = fileExplorerApplyPage(fileExplorerBeginPage(null, relativeReq), relativeReq, {
    path: 'src/a.ts', lines: ['relative stays relative'], truncated: false, totalLines: 1,
  });
  check('relative request keeps relative response', relativeApplied !== null && relativeApplied.lines.join(',') === 'relative stays relative');
  check(
    'absolute and relative cache keys are distinct identities',
    fileExplorerPageCacheKey('sess-a', '/workspace/notes.txt', 'file') !== fileExplorerPageCacheKey('sess-a', 'workspace/notes.txt', 'file'),
  );
  check(
    'display privacy never renders a raw absolute',
    fileExplorerDisplayPath('/workspace/notes.txt', '/workspace') === 'notes.txt'
      && fileExplorerDisplayPath('/elsewhere/a.ts', '/workspace') === '<workspace>/elsewhere/a.ts',
  );
}

// ---- line-offset behavior preserved ----
{
  const req = { sessionId: 'sess-a', path: 'a.ts', view: 'file' as const, cursor: 400, generation: 1 };
  const state = fileExplorerBeginPage(null, req);
  check('offset page starts at cursor', state.startCursor === 400);
  const applied = fileExplorerApplyPage(state, req, { path: 'a.ts', lines: ['line 401'], truncated: false });
  check('offset page keeps startCursor', applied !== null && applied.startCursor === 400);
  check('offset page keeps absolute line numbers', applied !== null && applied.lines.length === 1);
}

// ---- A(500) -> B -> A retains paged cache (not on the session slot) ----
{
  const sessionId = 'sess-a';
  const pageA1 = { sessionId, path: 'long.txt', view: 'file' as const, generation: 1 };
  const pageA2 = { sessionId, path: 'long.txt', view: 'file' as const, cursor: 400, generation: 2 };
  const pageB = { sessionId, path: 'src.c', view: 'file' as const, generation: 3 };
  const firstA = Array.from({ length: 400 }, (_, i) => `A${i + 1}`);
  const extraA = Array.from({ length: 100 }, (_, i) => `A${i + 401}`);

  let cache = emptyFileExplorerPageCache();
  cache = fileExplorerBeginCachedPage(cache, pageA1);
  cache = fileExplorerApplyCachedPage(cache, pageA1, {
    path: 'long.txt', lines: firstA, nextCursor: 400, truncated: true, sizeBytes: 1,
  });
  cache = fileExplorerBeginCachedPage(cache, pageA2);
  cache = fileExplorerApplyCachedPage(cache, pageA2, {
    path: 'long.txt', lines: extraA, truncated: false, sizeBytes: 1,
  });
  const loadedA = fileExplorerReadCachedPage(cache, sessionId, 'long.txt', 'file');
  check(
    'A load-more settles at 500',
    loadedA !== null && loadedA.lines.length === 500 && loadedA.nextCursor === undefined && loadedA.loading === false,
  );

  cache = fileExplorerBeginCachedPage(cache, pageB);
  cache = fileExplorerApplyCachedPage(cache, pageB, {
    path: 'src.c', lines: ['B1'], truncated: false,
  });
  const loadedB = fileExplorerReadCachedPage(cache, sessionId, 'src.c', 'file');
  check('B is independent of A', loadedB !== null && loadedB.lines.join(',') === 'B1');

  const restoredA = fileExplorerReadCachedPage(cache, sessionId, 'long.txt', 'file');
  check(
    'A->B->A retains 500 lines',
    restoredA !== null
      && restoredA.lines.length === 500
      && restoredA.lines[0] === 'A1'
      && restoredA.lines[499] === 'A500',
  );
  check('A->B->A does not reset first page', restoredA !== null && restoredA.nextCursor === undefined && restoredA.loading === false);
  check(
    'returning to A does not refetch first page',
    fileExplorerShouldFetchPage(restoredA, { sessionId, path: 'long.txt', view: 'file' }) === false,
  );
  check(
    'canonical ./long.txt shares cache key',
    fileExplorerPageCacheKey(sessionId, './long.txt', 'file') === fileExplorerPageCacheKey(sessionId, 'long.txt', 'file'),
  );
  check('file vs diff identities are isolated', fileExplorerReadCachedPage(cache, sessionId, 'long.txt', 'diff') === null);
  check('other session does not see A cache', fileExplorerReadCachedPage(cache, 'sess-b', 'long.txt', 'file') === null);

  const reloadB = { ...pageB, generation: 4 };
  cache = fileExplorerBeginCachedPage(cache, reloadB);
  cache = fileExplorerFailCachedPage(cache, reloadB, { message: 'boom' });
  const failedB = fileExplorerReadCachedPage(cache, sessionId, 'src.c', 'file');
  check('cached B failure keeps content', failedB !== null && failedB.lines.join(',') === 'B1' && failedB.loading === false && failedB.error?.message === 'boom' && failedB.error?.code === undefined);
}

// ---- module-lifetime registry: pages survive panel remount (A→B→A) ----
// The panel is keyed by sessionId in App, so component state alone would
// drop every settled page on remount. The registry is module-lifetime and
// keyed by session/path/view; write-through keeps it in sync with the
// panel's latest state, and absent keys are dropped on write.
{
  const sessionId = 'sess-a';
  const pageA = { sessionId, path: 'long.txt', view: 'file' as const, generation: 1 };
  const pageB = { sessionId, path: 'src.c', view: 'file' as const, generation: 2 };
  const linesA = Array.from({ length: 400 }, (_, i) => `A${i + 1}`);

  // First mount: seed from the (empty) registry, load A, write through.
  let cache = readModuleFileExplorerPageCache();
  check('registry starts empty', Object.keys(cache).length === 0);
  cache = fileExplorerBeginCachedPage(cache, pageA);
  cache = fileExplorerApplyCachedPage(cache, pageA, {
    path: 'long.txt', lines: linesA, nextCursor: 400, truncated: true, sizeBytes: 1,
  });
  writeModuleFileExplorerPageCache(cache);

  // Remount (A→B): a fresh panel seeds from the registry and sees A's page.
  const remounted = readModuleFileExplorerPageCache();
  const restoredA = fileExplorerReadCachedPage(remounted, sessionId, 'long.txt', 'file');
  check(
    'remount restores settled A page from registry',
    restoredA !== null && restoredA.lines.length === 400 && restoredA.lines[0] === 'A1' && restoredA.loading === false,
  );
  check(
    'remount does not refetch restored first page',
    fileExplorerShouldFetchPage(restoredA, { sessionId, path: 'long.txt', view: 'file' }) === false,
  );

  // B loads independently; A stays intact in the registry.
  let cacheB = fileExplorerBeginCachedPage(remounted, pageB);
  cacheB = fileExplorerApplyCachedPage(cacheB, pageB, { path: 'src.c', lines: ['B1'], truncated: false });
  writeModuleFileExplorerPageCache(cacheB);
  const afterB = readModuleFileExplorerPageCache();
  check('B page lands in registry', fileExplorerReadCachedPage(afterB, sessionId, 'src.c', 'file')?.lines.join(',') === 'B1');
  check('A page survives B write-through', fileExplorerReadCachedPage(afterB, sessionId, 'long.txt', 'file')?.lines.length === 400);

  // Dropping a key from the written cache removes it from the registry.
  const pruned = { ...afterB };
  delete pruned[fileExplorerPageCacheKey(sessionId, 'src.c', 'file')];
  writeModuleFileExplorerPageCache(pruned);
  check('pruned key leaves the registry', fileExplorerReadCachedPage(readModuleFileExplorerPageCache(), sessionId, 'src.c', 'file') === null);
  check('unrelated key survives pruning', fileExplorerReadCachedPage(readModuleFileExplorerPageCache(), sessionId, 'long.txt', 'file') !== null);
}

// ---- in-flight entries are sanitized on remount; reads are snapshots ----
// A loading entry can only belong to a panel that unmounted before its
// response arrived (the response is dropped by the session guard), so it
// can never settle. Keeping it would make shouldFetchPage return false
// forever; the panel sanitizes on mount so those keys refetch.
{
  const sessionId = 'sess-a';
  const pageA = { sessionId, path: 'long.txt', view: 'file' as const, generation: 1 };
  const pageB = { sessionId, path: 'src.c', view: 'file' as const, generation: 2 };

  // Mount 1: A begins a read (loading=true) and unmounts before any
  // reply; the wedged in-flight entry rides through the registry.
  let cache = readModuleFileExplorerPageCache();
  cache = fileExplorerBeginCachedPage(cache, pageA);
  writeModuleFileExplorerPageCache(cache);
  const inflight = fileExplorerReadCachedPage(readModuleFileExplorerPageCache(), sessionId, 'long.txt', 'file');
  check('in-flight entry persists in registry after unmount', inflight !== null && inflight.loading === true);
  check(
    'in-flight entry would block refetch without sanitize',
    fileExplorerShouldFetchPage(inflight, { sessionId, path: 'long.txt', view: 'file' }) === false,
  );

  // Session B mounts, loads and settles src.c while A stays wedged.
  let cacheB = fileExplorerBeginCachedPage(cache, pageB);
  cacheB = fileExplorerApplyCachedPage(cacheB, pageB, { path: 'src.c', lines: ['B1'], truncated: false });
  writeModuleFileExplorerPageCache(cacheB);
  const withB = readModuleFileExplorerPageCache();
  check('settled B lands beside the wedged A entry', fileExplorerReadCachedPage(withB, sessionId, 'src.c', 'file')?.lines.join(',') === 'B1');

  // Remount A: sanitize drops the loading entry (refetch allowed) and
  // keeps B's settled lines untouched.
  const sanitized = fileExplorerSanitizePageCache(readModuleFileExplorerPageCache());
  check('remount drops the wedged in-flight A entry', fileExplorerReadCachedPage(sanitized, sessionId, 'long.txt', 'file') === null);
  check(
    'remounted A key refetches',
    fileExplorerShouldFetchPage(fileExplorerReadCachedPage(sanitized, sessionId, 'long.txt', 'file'), { sessionId, path: 'long.txt', view: 'file' }) === true,
  );
  const keptB = fileExplorerReadCachedPage(sanitized, sessionId, 'src.c', 'file');
  check('remount keeps settled B lines', keptB !== null && keptB.lines.join(',') === 'B1' && keptB.loading === false);

  // The remount refetches A at a new generation; the unmounted panel's
  // generation-1 response arrives late and must not clobber it.
  const remountA = { ...pageA, generation: 7 };
  let cacheA = fileExplorerBeginCachedPage(sanitized, remountA);
  cacheA = fileExplorerApplyCachedPage(cacheA, pageA, { path: 'long.txt', lines: ['stale'], truncated: false });
  const staleA = fileExplorerReadCachedPage(cacheA, sessionId, 'long.txt', 'file');
  check('late stale response is generation-rejected', staleA !== null && staleA.loading === true && staleA.lines.length === 0);
  cacheA = fileExplorerApplyCachedPage(cacheA, remountA, { path: 'long.txt', lines: ['A1', 'A2'], truncated: false });
  const settledA = fileExplorerReadCachedPage(cacheA, sessionId, 'long.txt', 'file');
  check('fresh response settles the remounted key', settledA !== null && settledA.loading === false && settledA.lines.join(',') === 'A1,A2');
  check('settled A does not refetch after remount', fileExplorerShouldFetchPage(settledA, { sessionId, path: 'long.txt', view: 'file' }) === false);
  writeModuleFileExplorerPageCache(cacheA);

  // Reads return snapshot copies: mutating the snapshot never aliases the
  // registry object.
  const snapshot = readModuleFileExplorerPageCache();
  const before = Object.keys(snapshot).length;
  delete snapshot[fileExplorerPageCacheKey(sessionId, 'src.c', 'file')];
  check(
    'read returns a snapshot copy, not the registry object',
    Object.keys(readModuleFileExplorerPageCache()).length === before,
  );
}

// ---- page-cache helpers: fetch gate, stale apply, empty key ----
{
  check('empty cache read is null', fileExplorerReadCachedPage(emptyFileExplorerPageCache(), 'sess-a', 'a.ts', 'file') === null);
  check('cold key should fetch', fileExplorerShouldFetchPage(null, { sessionId: 'sess-a', path: 'a.ts', view: 'file' }) === true);

  const reqA = { sessionId: 'sess-a', path: 'a.ts', view: 'file' as const, generation: 1 };
  const reqB = { sessionId: 'sess-a', path: 'b.ts', view: 'file' as const, generation: 2 };
  let cache = fileExplorerBeginCachedPage(emptyFileExplorerPageCache(), reqA);
  const inflightA = fileExplorerReadCachedPage(cache, 'sess-a', 'a.ts', 'file');
  check('in-flight key does not refetch', fileExplorerShouldFetchPage(inflightA, { sessionId: 'sess-a', path: 'a.ts', view: 'file' }) === false);

  cache = fileExplorerApplyCachedPage(cache, reqA, {
    path: 'a.ts', lines: Array.from({ length: 400 }, (_, i) => `L${i + 1}`), nextCursor: 400, truncated: true,
  });
  const pagedA = fileExplorerReadCachedPage(cache, 'sess-a', 'a.ts', 'file');
  check('first-page window does not refetch', fileExplorerShouldFetchPage(pagedA, { sessionId: 'sess-a', path: 'a.ts', view: 'file' }) === false);
  check(
    'load-more cursor matching nextCursor should fetch',
    fileExplorerShouldFetchPage(pagedA, { sessionId: 'sess-a', path: 'a.ts', view: 'file', cursor: 400 }) === true,
  );
  check(
    'cursor inside loaded window does not fetch',
    fileExplorerShouldFetchPage(pagedA, { sessionId: 'sess-a', path: 'a.ts', view: 'file', cursor: 10 }) === false,
  );
  check(
    'other view identity should fetch',
    fileExplorerShouldFetchPage(pagedA, { sessionId: 'sess-a', path: 'a.ts', view: 'diff' }) === true,
  );

  cache = fileExplorerBeginCachedPage(cache, reqB);
  // A newer same-key request supersedes the settled A page; the old
  // generation-1 response must not clobber it.
  const reloadA = { ...reqA, generation: 3 };
  cache = fileExplorerBeginCachedPage(cache, reloadA);
  cache = fileExplorerApplyCachedPage(cache, reqA, { path: 'a.ts', lines: ['stale'], truncated: false });
  const afterStale = fileExplorerReadCachedPage(cache, 'sess-a', 'a.ts', 'file');
  const bInflight = fileExplorerReadCachedPage(cache, 'sess-a', 'b.ts', 'file');
  check('stale A apply cannot overwrite settled A', afterStale !== null && afterStale.lines.length === 400 && afterStale.lines[0] === 'L1');
  check('stale A apply cannot drop in-flight B', bInflight !== null && bInflight.loading === true && bInflight.path === 'b.ts');
}
// ---- settled error/empty pages refetch on remount (no sticky cache) ----
// A failed first-page read settles with error and zero lines; a
// successful-but-empty file settles with zero lines and no error; a
// transport failure latches transportIncomplete. That is not a cache hit
// and not an eager miss: epoch retry is the only automatic refetch.
{
  const sessionId = 'sess-a';
  const identity = { sessionId, path: 'a.ts', view: 'file' as const };
  const req = { sessionId, path: 'a.ts', view: 'file' as const, generation: 1, connectionEpoch: 3 };

  let cache = fileExplorerBeginCachedPage(emptyFileExplorerPageCache(), req);
  cache = fileExplorerFailCachedPage(cache, req, { message: 'read failed' });
  const failed = fileExplorerReadCachedPage(cache, sessionId, 'a.ts', 'file');
  check(
    'failed first-page read settles with error and no lines',
    failed !== null && failed.loading === false && failed.error?.message === 'read failed' && failed.error?.code === undefined && failed.lines.length === 0 && failed.transportIncomplete === false,
  );
  check('settled error page refetches on remount', fileExplorerShouldFetchPage(failed, identity) === true);

  cache = fileExplorerApplyCachedPage(cache, req, { path: 'a.ts', lines: [], truncated: false, totalLines: 0 });
  const empty = fileExplorerReadCachedPage(cache, sessionId, 'a.ts', 'file');
  check(
    'empty file settles with no lines and no error',
    empty !== null && empty.loading === false && empty.error === null && empty.lines.length === 0 && empty.transportIncomplete === false,
  );
  check('settled empty page refetches on remount', fileExplorerShouldFetchPage(empty, identity) === true);

  cache = fileExplorerBeginCachedPage(emptyFileExplorerPageCache(), req);
  cache = fileExplorerFailCachedPage(cache, req, null);
  const transport = fileExplorerReadCachedPage(cache, sessionId, 'a.ts', 'file');
  check(
    'transport failure settles without a message',
    transport !== null && transport.loading === false && transport.error === null && transport.lines.length === 0 && transport.transportIncomplete === true && transport.transportFailedEpoch === 3,
  );
  check('settled transport-incomplete page does not eager-refetch', fileExplorerShouldFetchPage(transport, identity) === false);
  check('same epoch does not retry transport-incomplete page', fileExplorerShouldRetryTransportPage(transport, 3) === false);
  check('new epoch retries transport-incomplete page once', fileExplorerShouldRetryTransportPage(transport, 4) === true);
  check('first-page transport retry keeps undefined cursor', fileExplorerTransportRetryCursor(transport, 200) === undefined);
  check('stamped epoch blocks a second same-epoch retry', fileExplorerShouldRetryTransportPage({ ...transport!, transportFailedEpoch: 4 }, 4) === false);
  check('in-flight page never epoch-retries', fileExplorerShouldRetryTransportPage({ loading: true, transportIncomplete: true }, 4) === false);

  const first = fileExplorerApplyPage(fileExplorerBeginPage(null, req), req, {
    path: 'a.ts', lines: ['one', 'two'], nextCursor: 2, truncated: true, totalLines: 4,
  });
  const moreReq = { ...req, cursor: 2, generation: 2 };
  const failedMore = fileExplorerFailPage(fileExplorerBeginPage(first, moreReq), moreReq, null);
  check('load-more transport stores failed cursor', failedMore?.transportCursor === 2);
  check('load-more transport retry reuses failed cursor', fileExplorerTransportRetryCursor(failedMore) === 2);
  const codedMoreReq = { ...req, cursor: 2, generation: 5 };
  const codedMore = fileExplorerFailPage(fileExplorerBeginPage(first, codedMoreReq), codedMoreReq, { message: 'later page failed', code: 'file_unavailable' });
  check(
    'load-more business fail preserves prior lines and structured code',
    codedMore !== null
      && codedMore.lines.join(',') === 'one,two'
      && codedMore.error?.message === 'later page failed'
      && codedMore.error?.code === 'file_unavailable'
      && codedMore.transportIncomplete === false,
  );

  const req2 = { ...req, generation: 3, connectionEpoch: 4 };
  cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(emptyFileExplorerPageCache(), req), req, { path: 'a.ts', lines: ['keep'], truncated: false, totalLines: 1 });
  cache = fileExplorerBeginCachedPage(cache, req2);
  cache = fileExplorerFailCachedPage(cache, req2, { message: 'reload failed', code: 'file_line_too_large' });
  const failedReload = fileExplorerReadCachedPage(cache, sessionId, 'a.ts', 'file');
  check('failed reload keeps prior content', failedReload !== null && failedReload.lines.join(',') === 'keep' && failedReload.error?.message === 'reload failed' && failedReload.error?.code === 'file_line_too_large' && failedReload.transportIncomplete === false);
  check('settled error page with content still refetches', fileExplorerShouldFetchPage(failedReload, identity) === true);

  cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(emptyFileExplorerPageCache(), req), req, { path: 'a.ts', lines: ['one'], truncated: false, totalLines: 1 });
  const content = fileExplorerReadCachedPage(cache, sessionId, 'a.ts', 'file');
  check('settled content page stays no-refetch', fileExplorerShouldFetchPage(content, identity) === false);
  check('settled content does not epoch-retry', fileExplorerShouldRetryTransportPage(content, 4) === false);
}

{
  const started = fileExplorerBeginDirState(undefined, 1);
  check('dir begin is loading with no transport latch', started.loading === true && started.transportIncomplete === false && started.error === null);
  const transport = fileExplorerFailDirState(started, null, 2, 1);
  check('dir transport settles loading without a local error', transport.loading === false && transport.error === null && transport.transportIncomplete === true && transport.transportFailedEpoch === 2);
  const rowsRequest = fileExplorerBeginDirState(undefined, 2);
  const withRows = fileExplorerApplyDirState(rowsRequest, [{ name: 'a.ts', path: 'a.ts', kind: 'file' }], false, 2);
  const keepRequest = fileExplorerBeginDirState(withRows, 3);
  const keepRows = fileExplorerFailDirState(keepRequest, null, 2, 3);
  check('dir transport keeps prior rows', keepRows.entries.length === 1 && keepRows.entries[0].name === 'a.ts' && keepRows.transportIncomplete === true && keepRows.loading === false);
  const businessRequest = fileExplorerBeginDirState(withRows, 4);
  const business = fileExplorerFailDirState(businessRequest, 'list failed', 2, 4);
  check('dir business error clears cached rows', business.entries.length === 0 && business.error === 'list failed' && business.transportIncomplete === false && business.loading === false);
  const removedRequest = fileExplorerBeginDirState(withRows, 7);
  const removed = fileExplorerFailDirState(removedRequest, 'directory is unavailable', 2, 7, 'directory_unavailable');
  check(
    'typed directory_unavailable drops rows and the error surface',
    removed.entries.length === 0 && removed.error === null && removed.errorCode === 'directory_unavailable' && removed.transportIncomplete === false && removed.loading === false,
  );
  const retried = fileExplorerBeginDirState(removed, 9);
  check('begin keeps the unavailable tombstone', retried.errorCode === 'directory_unavailable' && retried.loading === true);
  const retriedTransport = fileExplorerFailDirState(retried, null, 3, 9);
  check(
    'transport after unavailable keeps the tombstone and no rows',
    retriedTransport.errorCode === 'directory_unavailable' && retriedTransport.entries.length === 0 && retriedTransport.transportIncomplete === true,
  );
  const proseRequest = fileExplorerBeginDirState(withRows, 8);
  const prose = fileExplorerFailDirState(proseRequest, 'directory is unavailable', 2, 8);
  check(
    'untyped directory prose clears cached rows and stays visible',
    prose.entries.length === 0 && prose.error === 'directory is unavailable' && prose.errorCode === undefined && prose.transportIncomplete === false,
  );
  const appliedRequest = fileExplorerBeginDirState(withRows, 5);
  const applied = fileExplorerApplyDirState(appliedRequest, withRows.entries, false, 5);
  check('dir apply clears transport latch', applied.transportIncomplete === false && applied.loading === false && applied.error === null);
  const newer = fileExplorerBeginDirState(withRows, 6);
  const stale = fileExplorerApplyDirState(newer, [{ name: 'old.ts', path: 'old.ts', kind: 'file' }], false, 5);
  check('older dir response cannot overwrite newer generation', stale.generation === 6 && stale.loading === true && stale.entries[0]?.name === 'a.ts');
  const dirs = { '': transport, src: keepRows, lib: applied };
  check('only transport-stuck dirs are retry candidates', fileExplorerTransportIncompleteDirPaths(dirs).sort().join(',') === ',src');
  check('same epoch does not retry transport dirs', fileExplorerShouldRetryTransportDirs(dirs, 2) === false);
  check('new epoch retries transport dirs once', fileExplorerShouldRetryTransportDirs(dirs, 3) === true);
  check('new epoch returns both retryable dir paths', fileExplorerRetryableTransportDirPaths(dirs, 3).sort().join(',') === ',src');
  check('empty dirs never epoch-retry', fileExplorerShouldRetryTransportDirs({}, 2) === false);
  check('empty dir helper is settled success', fileExplorerEmptyDirState().transportIncomplete === false && fileExplorerEmptyDirState().loading === false);
  check('typed unavailable is not a transport retry', fileExplorerShouldRetryTransportDirs({ gone: removed }, 9) === false);
}

await (async () => {
  const live = new Map<string, Promise<unknown>>();
  const pending = Promise.withResolvers<void>();
  let creates = 0;
  const first = fileExplorerAcquireDirInflight('sess\0dir\0src', () => {
    creates += 1;
    return pending.promise;
  }, live);
  const replay = fileExplorerAcquireDirInflight('sess\0dir\0src', () => {
    creates += 1;
    return Promise.resolve();
  }, live);
  check('directory StrictMode replay reuses one in-flight promise', first === replay && creates === 1);
  check('directory in-flight is visible by identity', fileExplorerPeekDirInflight('sess\0dir\0src', live) === first);
  pending.resolve();
  await first;
  check('directory in-flight releases after settle', fileExplorerPeekDirInflight('sess\0dir\0src', live) === null);
})();
// ---- tab dedup: same path focuses existing ----
{
  const first = openFileTab([], 'src/a.ts', 10);
  check('first open adds tab', first.tabs.length === 1 && first.activePath === 'src/a.ts' && first.tabs[0].line === 10);
  const second = openFileTab(first.tabs, 'src/a.ts', 22);
  check('same path does not duplicate', second.tabs.length === 1 && second.activePath === 'src/a.ts' && second.tabs[0].line === 22);
  const third = openFileTab(second.tabs, 'src/b.ts');
  check('new path appends', third.tabs.length === 2 && third.activePath === 'src/b.ts');
  const closed = applyCloseTab({ tabs: third.tabs, activePath: third.activePath, mode: 'open' }, 'src/b.ts');
  check('close active selects previous', closed.tabs.length === 1 && closed.activePath === 'src/a.ts');
}

// ---- close/reopen preserves tabs; App toggle is the close path ----
{
  let slot = applyOpenPath(emptyFileExplorerSlot(), 'src/a.ts', 3);
  slot = applyOpenPath(slot, 'src/b.ts');
  check('open sets mode open', slot.mode === 'open' && slot.tabs.length === 2);
  const closed = applyDrawerMode(slot, 'closed');
  check('full close still keeps tabs', closed.mode === 'closed' && closed.tabs.length === 2 && closed.activePath === 'src/b.ts');
  const reopenedFromClose = applyDrawerMode(closed, 'open');
  check('toggle closed back to open keeps tabs', reopenedFromClose.mode === 'open' && reopenedFromClose.tabs.length === 2 && reopenedFromClose.activePath === 'src/b.ts');
}

{
  let closed = 0;
  let focused = 0;
  const toggle = { focus() { focused += 1; } };
  check(
    'non-Escape leaves the drawer open',
    fileExplorerHandleDocumentKeyDown({ key: 'Enter', preventDefault() {} }, () => { closed += 1; }, toggle) === false
      && closed === 0
      && focused === 0,
  );
  check(
    'Escape closes and restores focus to the Files toggle',
    fileExplorerHandleDocumentKeyDown({ key: 'Escape', preventDefault() {} }, () => { closed += 1; }, toggle) === true
      && closed === 1
      && focused === 1,
  );
  check(
    'Escape still closes when the toggle is missing',
    fileExplorerHandleDocumentKeyDown({ key: 'Escape', preventDefault() {} }, () => { closed += 1; }, null) === true
      && closed === 2
      && focused === 1,
  );
  check(
    'Escape on textarea does not close the drawer',
    fileExplorerHandleDocumentKeyDown(
      { key: 'Escape', preventDefault() {}, target: { tagName: 'TEXTAREA', isContentEditable: false } as HTMLElement },
      () => { closed += 1; },
      toggle,
    ) === false
      && closed === 2
      && focused === 1,
  );
}


// ---- Escape scope: panel-root toggle lookup ----
// The panel resolves its Escape focus owner from the panel root itself (one
// route per document after the URL-only cutover); a null root (SSR/tests)
// keeps the document lookup fallback.
{
  let focusedHost = 0;
  const hostToggle = { focus() { focusedHost += 1; } } as HTMLElement;
  const panelRoot = {
    querySelector(selectors: string): HTMLElement | null {
      return selectors === `#${FILE_EXPLORER_TOGGLE_ID}` ? hostToggle : null;
    },
  } as unknown as HTMLElement;

  const scoped = fileExplorerKeydownScope(panelRoot);
  check(
    'panel root is the Escape keydown scope',
    scoped === panelRoot,
  );

  const bare = fileExplorerKeydownScope(null);
  check(
    'null root keeps the document toggle fallback',
    bare === null,
  );

  let hostClosed = 0;
  const hostToggleEl = scoped?.querySelector(`#${FILE_EXPLORER_TOGGLE_ID}`) as { focus(): void } | null;
  check(
    'Escape closes and refocuses the host Files toggle',
    fileExplorerHandleDocumentKeyDown({ key: 'Escape', preventDefault() {} }, () => { hostClosed += 1; }, hostToggleEl) === true
      && hostClosed === 1
      && focusedHost === 1,
  );
}
// ---- session switch rebinding ----
{
  const bySession: Record<string, FileExplorerSessionSlot> = {
    A: applyOpenPath(emptyFileExplorerSlot(), 'a.ts', undefined, 'diff'),
    B: emptyFileExplorerSlot(),
  };
  const a = readFileExplorerSlot(bySession, 'A');
  const b = readFileExplorerSlot(bySession, 'B');
  check('session A has its tab', a.tabs.length === 1 && a.tabs[0].path === 'a.ts');
  check('session A preserves diff view after rebinding', a.tabs[0].view === 'diff');
  check('session B starts empty', b.tabs.length === 0 && b.mode === 'closed');
  check('unknown session is empty', readFileExplorerSlot(bySession, 'C').tabs.length === 0);
  check('null session is empty', readFileExplorerSlot(bySession, null).tabs.length === 0);
  const focused = applySelectTab(a, 'missing');
  check('select missing is a no-op', focused === a);
  const selectedAsFile = applySelectTab(a, 'a.ts', 'file');
  check('select updates persisted view', selectedAsFile.tabs[0].view === 'file');
}

// ---- basename + download filename scrub ----
{
  check('basename last segment', fileExplorerBasename('src/panels/FileExplorerPanel.tsx') === 'FileExplorerPanel.tsx');
  check('basename no slash', fileExplorerBasename('README.md') === 'README.md');
  check('download scrubs slashes and controls', fileExplorerDownloadFileName('a/../b\u0000c.ts') === 'a-..-b-c.ts');
  check('download empty becomes download', fileExplorerDownloadFileName('   ') === 'download');
}

// ---- tree type tone + search filter ----
{
  check('ts files are typescript tone', fileExplorerTreeTone('file', 'App.tsx') === 'typescript');
  check('js files are javascript tone', fileExplorerTreeTone('file', 'index.js') === 'javascript');
  check('directories stay dir tone', fileExplorerTreeTone('dir', 'src') === 'dir');
  check('unknown files stay generic', fileExplorerTreeTone('file', 'LICENSE') === 'file');
  check('typescript icon is a hidden path svg', (() => {
    const html = renderToStaticMarkup(createElement(FileTypeIcon, { tone: 'typescript' }));
    return html.includes('data-icon="typescript"') && html.includes('aria-hidden="true"') && html.includes('focusable="false"') && html.includes('<path ') && !html.includes('>TS<');
  })());
  check('javascript icon is a hidden path svg', (() => {
    const html = renderToStaticMarkup(createElement(FileTypeIcon, { tone: 'javascript' }));
    return html.includes('data-icon="javascript"') && html.includes('aria-hidden="true"') && html.includes('<path ') && !html.includes('>JS<');
  })());
  check('empty query matches every row', fileExplorerTreeQueryMatches('  ', 'App.tsx', 'src/App.tsx'));
  check('query matches basename case-insensitively', fileExplorerTreeQueryMatches('APP', 'App.tsx', 'src/App.tsx'));
  check('query matches path segment', fileExplorerTreeQueryMatches('src/', 'App.tsx', 'src/App.tsx'));
  check('unrelated query misses', fileExplorerTreeQueryMatches('python', 'App.tsx', 'src/App.tsx') === false);
  check(
    'loaded descendant match finds nested file',
    fileExplorerLoadedDescendantMatches('src', 'a.ts', {
      src: { entries: [{ name: 'a.ts', path: 'src/a.ts', kind: 'file' }] },
    }),
  );
  check(
    'loaded descendant match ignores sibling trees',
    fileExplorerLoadedDescendantMatches('src', 'a.ts', {
      docs: { entries: [{ name: 'a.ts', path: 'docs/a.ts', kind: 'file' }] },
    }) === false,
  );
  check('normalize trims and lowercases', fileExplorerNormalizeTreeQuery('  TsX ') === 'tsx');
}

// ---- file conversation: strict snapshot, ask surface, draft close ack ----
{
  const expected = { conversationId: 'conv-1', path: 'src/a.ts' };
  const entry = {
    id: 'e1',
    kind: 'user',
    role: 'user',
    text: 'hello',
    isError: false,
    isPartial: false,
  };
  const base = {
    conversationId: 'conv-1',
    path: 'src/a.ts',
    streaming: false,
    status: 'Side chat ready',
    entries: [entry],
    accepted: true,
  };
  const parsed = normalizeFileConversationSnapshot(base, expected);
  check('strict snapshot keeps typed status and accepted', parsed !== null && parsed.status === 'Side chat ready' && parsed.accepted === true && parsed.entries.length === 1);
  check('malformed entry rejects the whole snapshot', normalizeFileConversationSnapshot({ ...base, entries: [entry, { id: 'bad' }] }, expected) === null);
  check('non-boolean entry flags reject the whole snapshot', normalizeFileConversationSnapshot({ ...base, entries: [{ ...entry, isError: 'no' }] }, expected) === null);
  check('non-string status rejects the snapshot', normalizeFileConversationSnapshot({ ...base, status: true }, expected) === null);
  check('non-boolean accepted rejects the snapshot', normalizeFileConversationSnapshot({ ...base, accepted: 'yes' }, expected) === null);
  check('absent accepted is not success', fileConversationPromptAccepted({}) === false && fileConversationPromptAccepted({ accepted: true, busy: true }) === false);
  check('accepted true and not busy is success', fileConversationPromptAccepted({ accepted: true }) === true && fileConversationPromptAccepted({ accepted: true, busy: false }) === true);
  check('included range end before start rejects the snapshot', normalizeFileConversationSnapshot({
    ...base,
    context: { truncated: false, includedRange: { startLine: 8, endLine: 2 } },
  }, expected) === null);
  check('included range accepts ordered lines', normalizeFileConversationSnapshot({
    ...base,
    context: { truncated: true, includedRange: { startLine: 2, endLine: 8 } },
  }, expected)?.context?.includedRange?.endLine === 8);

  const emptyView = emptyFileExplorerViewState();
  const asked = fileExplorerWithPathSurface(emptyView, 'src/a.ts', 'ask');
  check('ask survives on a non-preview file', fileExplorerContentSurface(asked, 'src/a.ts', 'file') === 'ask');
  check('ask survives a later source setter', fileExplorerContentSurface(fileExplorerWithPathSurface(asked, 'src/a.ts', 'source'), 'src/a.ts', 'file') === 'source');
  const restored = fileExplorerWithPathSurface(fileExplorerWithPathSurface(asked, 'src/a.ts', 'source'), 'src/a.ts', 'ask');
  check('ask can be restored on a non-preview file', fileExplorerContentSurface(restored, 'src/a.ts', 'file') === 'ask');
  const drafted = fileExplorerWithConversationDraft(restored, 'src/a.ts', 'keep me');
  check('draft lives in the existing view store', fileExplorerConversationDraft(drafted, 'src/a.ts') === 'keep me');
  const reopened = openFileTab([{ path: 'src/a.ts', view: 'file', conversationId: 'conv-1' }], 'src/a.ts');
  check('reopen keeps the tab conversation id', reopened.tabs[0].conversationId === 'conv-1');

  const tones = ['dir', 'typescript', 'javascript', 'rust', 'solidity', 'python', 'go', 'markdown', 'json', 'css', 'html', 'image', 'config', 'shell', 'document', 'file'] as const;
  const icons = tones.map((tone) => renderToStaticMarkup(createElement(FileTypeIcon, { tone, expanded: tone === 'dir' ? false : undefined })));
  const openDir = renderToStaticMarkup(createElement(FileTypeIcon, { tone: 'dir', expanded: true }));
  const seenPaths: Record<string, true> = {};
  let iconsAccessible = true;
  let iconsDistinct = true;
  for (let index = 0; index < icons.length; index += 1) {
    const html = icons[index];
    const geometry = (html.match(/ d="[^"]*"/g) ?? []).join('|');
    if (!html.includes('<svg') || !html.includes('class="file-type-icon"') || !html.includes('aria-hidden="true"') || !html.includes('focusable="false"') || !html.includes('width="16"') || !html.includes('height="16"') || !html.includes('<path ') || !html.includes(`data-icon="${tones[index]}"`) || geometry.length === 0 || /<text[\s>]/.test(html)) iconsAccessible = false;
    if (seenPaths[geometry]) iconsDistinct = false;
    seenPaths[geometry] = true;
  }
  check('every tone renders an accessible path icon', iconsAccessible);
  check('tone icon paths are geometrically distinct', iconsDistinct);
  const openDirPaths = (openDir.match(/ d="[^"]*"/g) ?? []).join('|');
  const closedDirPaths = (icons[0].match(/ d="[^"]*"/g) ?? []).join('|');
  check('expanded directory paths differ from the closed folder', openDir.includes('data-icon="dir"') && openDirPaths.length > 0 && openDirPaths !== closedDirPaths);
  const solidityIcon = renderToStaticMarkup(createElement(FileTypeIcon, { tone: 'solidity' }));
  check(
    'solidity stays a solidity tone with a stacked diamond icon',
    fileExplorerTreeTone('file', 'Token.sol') === 'solidity'
      && solidityIcon.includes('data-icon="solidity"')
      && (solidityIcon.match(/<path /g) ?? []).length === 3
      && !solidityIcon.includes('>Sol<')
      && !/<text[\s>]/.test(solidityIcon),
  );

  check('close ack requires the requested id and closed true', fileConversationCloseAckValid({ conversationId: 'conv-1', closed: true }, 'conv-1') === true);
  check('undefined success is not a close ack', fileConversationCloseAckValid(undefined, 'conv-1') === false && fileConversationCloseAckValid({ conversationId: 'conv-1', closed: false }, 'conv-1') === false && fileConversationCloseAckValid({ closed: true }, 'conv-1') === false && fileConversationCloseAckValid({ conversationId: 'other', closed: true }, 'conv-1') === false);
  const prev = { sess: { tabs: [{ path: 'src/a.ts', view: 'file' as const, conversationId: 'conv-1' }], activePath: 'src/a.ts', mode: 'open' as const } };
  const patch = { slots: {}, evictedSessions: [], droppedPathsBySession: { sess: ['src/a.ts'] } };
  const errors: string[] = [];
  const sent: string[] = [];
  const undefinedAck = closeFileConversationsDroppedBySlotPatch(prev, patch, () => {
    sent.push('undefined');
    return Promise.resolve(undefined);
  }, (message) => errors.push(message));
  check('cleanup returns a promise', typeof undefinedAck.then === 'function');
  pending.push(undefinedAck.then(() => {
    check('undefined close success is rejected by the dispatcher', sent.join(',') === 'undefined' && errors.length === 1 && errors[0] === 'unexpected close acknowledgement for conversation conv-1');
    const notFound = closeFileConversationsDroppedBySlotPatch(prev, patch, () => {
      sent.push('not-found');
      return Promise.reject(Object.assign(new Error('gone'), { rpcCode: 'file_conversation_not_found' }));
    }, (message) => errors.push(`fatal:${message}`));
    return notFound.then(() => {
      check('exact not-found close is nonfatal', errors.length === 1 && sent.join(',') === 'undefined,not-found');
      return closeFileConversationsDroppedBySlotPatch(prev, patch, () => {
        sent.push('other');
        return Promise.reject(Object.assign(new Error('boom'), { rpcCode: 'file_conversation_binding_mismatch' }));
      }, (message) => errors.push(message));
    }).then(() => {
      check('other typed close failure propagates', errors.length === 2 && errors[1] === 'boom');
    });
  }));
}


// ---- hidden-anchor download uses session export URL ----
{
  const clicks: string[] = [];
  const removed: string[] = [];
  const doc = {
    createElement: (tag: 'a') => {
      check('creates an anchor', tag === 'a');
      const node = {
        href: '',
        download: '',
        hidden: false,
        click() { clicks.push(`${this.href}|${this.download}|${this.hidden}`); },
        remove() { removed.push(this.download); },
      };
      return node;
    },
    body: { appendChild() {} },
  };
  triggerFileExplorerDownload(
    'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee',
    'note.md',
    { downloadOrigin: 'http://127.0.0.1:3000', document: doc },
  );
  check('download clicked once', clicks.length === 1, clicks.join(','));
  check('App same-origin href uses location.origin', clicks[0] === 'http://127.0.0.1:3000/download/session/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee|note.md|true');
  check('download filename scrubbed onto hidden anchor', clicks[0].endsWith('|note.md|true'));
  check('download anchor removed', removed.join(',') === 'note.md');

  clicks.length = 0;
  removed.length = 0;
  triggerFileExplorerDownload(
    'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee',
    'note.md',
    { downloadOrigin: 'https://runner.example:4000', document: doc },
  );
  check('Chat remote href uses the runner origin', clicks[0] === 'https://runner.example:4000/download/session/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee|note.md|true');
  clicks.length = 0;
  triggerFileExplorerDownload(
    'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee',
    'note.md',
    { downloadOrigin: 'http://runner.example:4000', document: doc },
  );
  check('opposite scheme keeps the exact runner origin', clicks[0] === 'http://runner.example:4000/download/session/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee|note.md|true');
  check('download never Authorization-fetches', !clicks[0].includes('Authorization') && !clicks.join(',').includes('OPTIONS'));
}

// ---- panel markup: two-viewport Files explorer (SSR structure only) ----
// SSR asserts selectors, the static Files title, the route-prefixed drawer
// close, selected content without download/path chrome, inline-status source
// wiring, and that rejected tab/Changed-files chrome is gone. It does NOT
// prove geometry.
{
  const commands: Record<string, unknown>[] = [];
  const opened: Array<{ path: string; line?: number; view?: 'file' | 'diff' }> = [];
  const sendCommand = (command: Record<string, unknown>) => {
    commands.push(command);
    if (command.type === 'file_explorer_changes') {
      return Promise.resolve({
        isRepo: true,
        files: [{ path: 'src/a.ts', status: 'modified' }],
      });
    }
    if (command.type === 'file_explorer_list') {
      return Promise.resolve({
        entries: [
          { name: 'src', path: 'src', kind: 'dir' },
          { name: 'a.ts', path: 'src/a.ts', kind: 'file', sizeBytes: 12 },
        ],
        truncated: false,
      });
    }
    if (command.type === 'file_explorer_read') {
      return Promise.resolve({ path: 'src/a.ts', lines: ['export const x = 1;'], sizeBytes: 18, truncated: false });
    }
    if (command.type === 'file_explorer_diff') {
      return Promise.resolve({ path: 'src/a.ts', lines: ['@@ -1 +1 @@', '-old', '+new'], truncated: false });
    }
    return Promise.resolve({});
  };
  const html = renderToStaticMarkup(createElement(FileExplorerPanel, {
    sendCommand,
    sessionId: 'sess-a',
    slot: {
      tabs: [{ path: 'src/a.ts', line: 450, view: 'file' }],
      activePath: 'src/a.ts',
      mode: 'open',
    },
    hostAuthority: '127.0.0.1:1',
    pageProtocol: 'http:',
    connectionEpoch: 0,
    onOpenPath: (path, line, view) => { opened.push({ path, line, view }); },
    onClose: () => {},
  }));
  check('drawer uses panel-drawer family', html.includes('class="panel-drawer file-explorer"'), html.slice(0, 200));
  assertTwoViewportStructure('open', html);
  check('selected path is not a content-head tab', !html.includes('file-explorer__content-path') && !html.includes('>Download</button>'));
  check('drawer close is the one full close', html.includes('id="file-explorer-drawer-close-btn"') && html.includes('aria-label="Close file explorer"') && !html.includes('file-explorer-hide-btn') && !html.includes('file-explorer-close-btn'));
  check('aria-label File explorer is intact', html.includes('aria-label="File explorer"'));

  const empty = renderToStaticMarkup(createElement(FileExplorerPanel, {
    sendCommand,
    sessionId: 'sess-b',
    slot: emptyFileExplorerSlot(),
    hostAuthority: '127.0.0.1:1',
    pageProtocol: 'http:',
    connectionEpoch: 0,
    onOpenPath: () => {},
    onClose: () => {},
  }));
  assertTwoViewportStructure('empty', empty);
  check('empty slot keeps the Files header, hint copy removed without replacement', empty.includes('>Files<') && !empty.includes('Open a file from markdown'));
}

// ---- SSR loading pane: selected path, cold cache, session bound ----
// Effects never run under renderToStaticMarkup, so the 151ms delayed-loading
// gate hides every indicator at immediate SSR: the stable content viewport
// stays rendered (never blank) with no spinner/loading/shimmer, and the
// drawer close remains the one full close.
{
  const html = renderToStaticMarkup(createElement(FileExplorerPanel, {
    sendCommand: () => Promise.resolve({}),
    sessionId: 'sess-loading',
    slot: {
      tabs: [{ path: 'src/a.ts', view: 'file' }],
      activePath: 'src/a.ts',
      mode: 'open',
    },
    hostAuthority: '127.0.0.1:1',
    pageProtocol: 'http:',
    connectionEpoch: 0,
    onOpenPath: () => {},
    onClose: () => {},
  }));
  check('cold selected path renders no spinner or loading marker at SSR', !html.includes('file-explorer__spinner') && !html.includes('file-explorer__loading'));
  check('cold selected path renders no delayed shimmer before the client gate', !html.includes('file-explorer__pane-shimmer'));
  check('cold selected path keeps the stable content viewport', html.includes('file-explorer__content-viewport') && !html.includes('file-explorer__code') && !html.includes('file-explorer__markdown'));
  check('drawer close is present while loading', html.includes('id="file-explorer-drawer-close-btn"') && html.includes('aria-label="Close file explorer"'));
}

// ---- changed-file status aria-label redaction (hostile credential path) ----
// The diff status button's accessible name is backend-path-derived: a
// credential-shaped filename (sk-…) must never reach the accessibility tree
// raw. The label goes through safeText exactly like the visible title, while
// the click argument keeps the raw business path (source assertion below).
{
  const hostilePath = 'src/sk-Ab3456789CdEf012GhIjKlMnOp.ts';
  const html = renderToStaticMarkup(createElement(ChangeStatusButton, {
    change: { path: hostilePath, status: 'modified' },
    onOpenDiff: () => {},
  }));
  check('raw credential-shaped path absent from diff button markup', !html.includes(hostilePath), html);
  check('diff button aria-label is redacted', html.includes('aria-label="Open diff for src/[REDACTED].ts"'), html);
  check('diff button visible title is redacted', html.includes('title="Open diff for src/[REDACTED].ts"'), html);
}

// ---- selected hostile path never renders as chrome ----
// File selection is a highlighted tree row only: the content head no longer
// shows the active path, so a credential-shaped filename cannot leak there.
{
  const hostilePath = 'src/sk-Ab3456789CdEf012GhIjKlMnOp.ts';
  const html = renderToStaticMarkup(createElement(FileExplorerPanel, {
    sendCommand: () => Promise.resolve({}),
    sessionId: 'sess-redact',
    slot: {
      tabs: [{ path: hostilePath, view: 'file' }],
      activePath: hostilePath,
      mode: 'open',
    },
    hostAuthority: '127.0.0.1:1',
    pageProtocol: 'http:',
    connectionEpoch: 0,
    onOpenPath: () => {},
    onClose: () => {},
  }));
  check('raw credential-shaped path absent from selected-file chrome', !html.includes(hostilePath), html);
  check('no visible Download chrome for a selected hostile path', !html.includes('>Download</button>') && !html.includes('file-explorer__download'), html);
}

{
  const here = dirname(fileURLToPath(import.meta.url));
  const app = readFileSync(join(here, '../src/App.tsx'), 'utf8');
  const panel = readFileSync(join(here, '../src/panels/FileExplorerPanel.tsx'), 'utf8');
  const css = readFileSync(join(here, '../src/styles.css'), 'utf8');
  const mdStart = app.indexOf('const onMarkdownOpenPath');
  const mdEnd = app.indexOf('useEffect(() => subscribeMarkdownOpenPath(onMarkdownOpenPath)', mdStart);
  const markdownHandler = mdStart >= 0 && mdEnd > mdStart ? app.slice(mdStart, mdEnd) : '';
  check('Files toggle is independent of activePanel', app.includes('id="file-explorer-toggle-btn"') && app.includes('toggleFileExplorer'));
  check('opening a feature panel closes the file explorer', app.includes("if (sid) patchFileExplorer(sid, (slot) => applyDrawerMode(slot, 'closed'))"));
  check('opening the file explorer closes the feature panel', app.includes("if (opening) setActivePanel('')"));
  check('drawer close is the one in-drawer full close', panel.includes(`FILE_EXPLORER_CLOSE_BTN_ID = 'file-explorer-drawer-close-btn'`) && panel.includes('aria-label="Close file explorer"') && panel.includes('title="Close file explorer"') && panel.includes('className="file-explorer__close"') && !panel.includes('className="file-explorer__title-button"') && !panel.includes('className="file-explorer__hide"') && !panel.includes('FILE_EXPLORER_HIDE_BTN_ID') && panel.includes('const closeDrawer = useCallback') && panel.includes('onClick={closeDrawer}'));
  check('App Files toggle reopens the drawer', app.includes(`id="${FILE_EXPLORER_TOGGLE_ID}"`) && panel.includes(`FILE_EXPLORER_TOGGLE_ID = '${FILE_EXPLORER_TOGGLE_ID}'`));
  check('Files toggle closes and reopens the same session slot', app.includes("slot.mode === 'closed' ? 'open' : 'closed'"));
  check('get_session_stats auxiliary is explicit per call', app.includes("auxiliary: opts?.auxiliary === true") && app.includes("sendCommand(sessionStatsCommand(sid), undefined, { auxiliary: true })"));
  check('transcript rows carry their owning session', app.includes('data-session-id={sessionId ?? \'\'}') && app.includes('data-session-id={sid}'));
  check('drawer open binds the rendered session', app.includes('onOpenPath={(path, line, view) => openFileInDrawer(path, line, sessionId ?? undefined, view)}'));
  check('drawer close still binds the rendered session', app.includes("applyDrawerMode(slot, 'closed')"));
  check('panel drops collapse/expand header props', !panel.includes('onCollapse') && !panel.includes('onExpand') && !panel.includes('file-explorer--collapsed'));
  check('Escape document listener restores Files toggle focus', panel.includes('fileExplorerHandleDocumentKeyDown') && panel.includes('document.addEventListener(\'keydown\''));


  check('collapsed CSS class is gone', !css.includes('.file-explorer--collapsed') && !css.includes('.file-explorer__icon-btn'));
  check(
    'type-color rules follow the base row color so TS/JS stay distinct',
    css.indexOf('.file-explorer__row {\n') < css.indexOf('.file-explorer__row--typescript { color: #79b8ff; }')
      && css.indexOf('.file-explorer__row--typescript { color: #79b8ff; }') < css.indexOf('.file-explorer__row--oversized:disabled'),
  );
  check(
    'inline change status lives on tree rows',
    panel.includes('file-explorer__status')
      && panel.includes('file-explorer__status--')
      && panel.includes('data-status=')
      && panel.includes('Open diff for'),
  );
  check(
    'status chip renders AFTER its row button (trailing column, tab order row → chip)',
    panel.includes(`\${row.change ? ' file-explorer__row--has-status' : ''}`)
      && panel.indexOf('<span className="file-explorer__row-path">{safeText(row.name)}</span>')
        < panel.indexOf('{row.change && <ChangeStatusButton'),
  );
  check(
    'directory status letter uses the same trailing column without a count',
    panel.includes('DirStatusBadge')
      && panel.includes('{row.changeCount ? <DirStatusBadge tone={row.changeTone} /> : null}')
      && panel.includes('fileExplorerStatusLetter')
      && panel.includes('diffTreePaddingLeft(depth)')
      && !panel.includes('count > 1')
      && !panel.includes('file-explorer__dir-count')
      && css.includes('.file-explorer__row--dir { color: var(--muted); font-weight: 600; }')
      && !css.includes('.file-explorer__row--dir { color: var(--accent);'),
  );
  check(
    'toggleDir binds updateFileExplorerViewCache from the fileExplorer import',
    panel.includes('updateFileExplorerViewCache,')
      && panel.includes('updateFileExplorerViewCache(viewOwnerKey, (view) => {'),
  );
  check(
    'hunk cards do not shrink inside the column-flex diff scroller',
    css.includes('.file-explorer__diff {\n  display: flex;\n  flex-direction: column;')
      && css.includes('.file-explorer__hunk {\n')
      && css.includes('flex: none;'),
  );
  check(
    'directory status letters do not capture clicks or mobile 44px height',
    css.includes('.file-explorer__item > .file-explorer__status[aria-hidden="true"]')
      && css.includes('pointer-events: none;')
      && css.includes('.file-explorer__status:not([aria-hidden="true"])'),
  );
  check(
    'code pane uses a callback ref so HTMLElement can bind div and pre',
    panel.includes('const setCodeRef = useCallback((node: HTMLElement | null) => {')
      && panel.includes('ref={setCodeRef}'),
  );
  check(
    'trailing status column CSS: anchored item, absolute chip, desktop + mobile reserve',
    /\.file-explorer__item\s*\{[^}]*position:\s*relative/.test(css)
      && /\.file-explorer__item > \.file-explorer__status\s*\{[^}]*position:\s*absolute/.test(css)
      && /\.file-explorer__item > \.file-explorer__status\s*\{[^}]*right:\s*6px/.test(css)
      && /\.file-explorer__row--has-status\s*\{[^}]*padding-right:\s*calc\(1\.4em \+ 10px\)/.test(css)
      && css.includes('.file-explorer__row--has-status {\n    padding-right: calc(44px + 10px);\n  }'),
  );
  check(
    'changed-file aria-label goes through safeText with raw click identity',
    panel.includes('aria-label={safeText(disabledReason ?? `Open diff for ${displayPath(change.path)}`)}')
      && !/aria-label=\{`Open diff for \$\{change\.path\}`\}/.test(panel)
      && panel.includes('onClick={disabledReason == null ? () => onOpenDiff(change.path) : undefined}'),
  );
  check('no stacked Changed files overlay in panel source', !panel.includes('Changed files') && !panel.includes('FILE TABS'));
  check('no tab-chip chrome in panel source', !panel.includes('file-explorer__tab') && !panel.includes('role="tablist"'));
  check(
    'two-viewport classes exist in CSS',
    css.includes('.file-explorer__tree-viewport')
      && css.includes('.file-explorer__content-viewport')
      && css.includes('.file-explorer__divider'),
  );
  check('file document panes reserve scrollbar gutter so the thumb cannot cover source',
    /\.file-explorer__code \{[\s\S]*?scrollbar-gutter:\s*stable/.test(css)
    && /\.file-explorer__markdown \{[\s\S]*?scrollbar-gutter:\s*stable/.test(css));
  check('layout wrap exists in panel source', panel.includes('data-testid="file-explorer-layout"') && panel.includes('data-layout={fileExplorerBodyLayout(isNarrow, treeCollapsed)}'));
  check('header testid exists in panel source', panel.includes('data-testid="file-explorer-header"'));
  check('obsolete title-button chrome is gone', css.includes('.file-explorer__title {') && !css.includes('.file-explorer__title-button'));
  check('visible Download chrome is gone', !panel.includes('file-explorer__download') && !panel.includes('>Download</button>') && !css.includes('.file-explorer__download'));
  check('dir Retry refetches instead of toggling expansion', panel.includes('onRetryDir={loadDir}') && panel.includes('onClick={() => onRetryDir(row.path)}'));
  check('reopen reloads normal dirs but preserves same-epoch transport failures for Retry',
    panel.includes("if (!paths.includes('')) paths.unshift('')")
      && panel.includes('if (dirs[path]?.transportIncomplete) continue')
      && panel.includes('for (const path of paths)'));
  check('tree scroll restores after root listing settles', panel.includes('node.scrollTop = resolvedView.treeScrollTop') && panel.includes('if (!root || root.loading) return'));
  check('App unmounts drawer and resizer with fileExplorerOpen', (app.match(/\{fileExplorerOpen && \(/g) ?? []).length >= 2);
  check('shared inflight acquire precedes send', panel.includes('fileExplorerAcquireInflight(identity') && panel.includes('fileExplorerSettleInflightCache'));
  check('shared inflight releases only in finally', panel.includes('.finally(() => fileExplorerReleaseInflight(identity))'));
  check('unmount does not cancel live inflight', panel.includes('aliveRef.current = false') && panel.includes('.finally(() => fileExplorerReleaseInflight(identity))') && !panel.includes('inflightRef'));
  check('request generation is a global monotonic counter', panel.includes('nextFileExplorerRequestGeneration()') && !panel.includes('requestGenerationRef'));
  check('panel reads the page cache via useSyncExternalStore', panel.includes('useSyncExternalStore') && panel.includes('subscribeFileExplorerPageCache') && panel.includes('getFileExplorerPageCacheSnapshot'));
  check('gitOnly and changed rows open the canonical change.path as a diff', panel.includes('if (row.gitOnly || row.change) onOpenDiff(row.change?.path ?? row.path)'));
  check('download is a hidden-anchor navigation', panel.includes('triggerFileExplorerDownload(parsed.downloadId, parsed.fileName') && panel.includes('downloadOrigin:'));
  check('App download uses location.origin', app.includes('downloadOrigin={typeof location !== \'undefined\' ? location.origin'));
  check('infinite scroll uses the shared threshold helper', panel.includes('fileExplorerShouldLoadNext') && panel.includes('onContentScroll'));
  check('merged Git tree is consumed once by WorkspaceTree', panel.includes('fileExplorerMergeTree(path, node?.entries ?? [], changes, changeIndex, unavailable)') && panel.includes('fileExplorerBuildChangeIndex(visibleChanges)') && !panel.includes('orphanChanges') && !panel.includes('data-orphans'));
  check(
    'App markdown probe routes through the ownership helper and toasts once on local ownership (retry-gated on a stale cwd)',
    markdownHandler.includes("markdownOpenRejectionOwnership(error) !== 'local'")
      && markdownHandler.includes('toast(`open file failed: ${detail}`, true);')
      && markdownHandler.includes("if (retried || (error as PendingCommandError | null)?.rpcCode !== 'file_unavailable') {")
      && !markdownHandler.includes('/file is unavailable/')
      && !/file is unavailable/i.test(markdownHandler)
      && (markdownHandler.match(/toast\(`open file failed:/g) ?? []).length === 2,
    markdownHandler.slice(-500),
  );
  check('static Files title has no title-button focus ring', !css.includes('.file-explorer__title-button:focus-visible') && css.includes('.file-explorer__close:focus-visible'));
  check(
    'file explorer top reads the shared header-height var (no fixed 90vh pin)',
    /\.file-explorer\s*\{[^}]*top\s*:\s*var\(--app-header-height\)[^}]*height\s*:\s*auto[^}]*max-height\s*:\s*none/.test(css),
  );
  check(
    'full-app expand keeps its names, hides the sibling header, and zeros the drawer top',
    fileExplorerControlId(FILE_EXPLORER_TOGGLE_ID, 'expand-btn') === 'file-explorer-expand-btn'
      && panel.includes('onToggleExpanded')
      && panel.includes("expanded ? 'Restore file explorer' : 'Expand file explorer to the full app area'")
      && app.includes('fileExplorerFullscreenActive')
      && app.includes('hidden={filesFullscreen}')
      && css.includes('header[hidden]')
      && /\.app-layout--files-expanded \.file-explorer \{[^}]*top:\s*0/.test(css)
      && css.includes('.app-layout--files-expanded .session-sidebar')
      && css.includes('.app-layout--files-expanded .app-main')
      && /\.app-layout--files-expanded \.file-explorer \{[^}]*width: 100vw;/.test(css)
      && css.includes('.file-explorer__expand-btn.is-active'),
  );
  check(
    'App markdown probe names the bounded referenced path only on the post-retry missing-file verdict',
    markdownHandler.includes('markdownOpenMissingFileVerdict(retried, error)')
      && markdownHandler.includes('markdownOpenMissingFileToast(path)'),
    markdownHandler.slice(-700),
  );
  check(
    'inner split CSS var is the tree width authority',
    css.includes('--file-explorer-tree-pct: 34%')
      && css.includes('flex: 0 0 var(--file-explorer-tree-pct, 34%)')
      && /min-width:\s*22%/.test(css)
      && /max-width:\s*55%/.test(css),
  );
  check(
    'desktop divider is a visible column-resize separator',
    /\.file-explorer__divider\s*\{[^}]*cursor:\s*col-resize/.test(css)
      && /\.file-explorer__divider:focus-visible\s*\{[^}]*outline:\s*2px solid var\(--accent\)/.test(css),
  );
  const mobileFilesStart = css.indexOf('@media (max-width: 720px) {\n  .file-explorer {');
  const mobileFiles = mobileFilesStart >= 0 ? css.slice(mobileFilesStart, mobileFilesStart + 2200) : '';
  check(
    'phone ≤720 exclusive panes fill the body without a 42% stacked tree',
    /flex:\s*1 1 auto/.test(mobileFiles)
      && /max-height:\s*none/.test(mobileFiles)
      && mobileFiles.includes('.file-explorer__body:not(.file-explorer__body--tree-collapsed) .file-explorer__content-viewport')
      && /@media \(max-width: 720px\)[\s\S]*\.file-explorer__body:not\(\.file-explorer__body--tree-collapsed\) \.file-explorer__content-viewport \{[\s\S]*?display:\s*none;/.test(css)
      && /@media \(max-width: 720px\)[\s\S]*\.file-explorer__body--tree-collapsed \.file-explorer__tree-viewport \{[\s\S]*?display:\s*none;/.test(css)
      && !/max-height:\s*42%/.test(mobileFiles)
      && !/flex:\s*0 0 42%/.test(mobileFiles)
      && !/file-explorer__(?:tree|content)-viewport[\s\S]{0,240}max-height:\s*42%/.test(css)
      && !/file-explorer__(?:tree|content)-viewport[\s\S]{0,240}flex:\s*0 0 42%/.test(css)
      && panel.includes('data-layout={fileExplorerBodyLayout(isNarrow, treeCollapsed)}')
      && panel.includes("className={`file-explorer__body${treeCollapsed ? ' file-explorer__body--tree-collapsed' : ''}`}")
      && panel.includes('hidden={treeCollapsed}')
      && panel.includes('hidden={isNarrow && !treeCollapsed}'),
  );
  check(
    'fold hide: [hidden] display:none wins over display:flex on tree/content/divider',
    /\.file-explorer__tree-viewport,\s*\n\.file-explorer__content-viewport \{[\s\S]*?display:\s*flex;/.test(css)
      && /\.file-explorer__tree-viewport\[hidden\],\s*\n\.file-explorer__content-viewport\[hidden\],\s*\n\.file-explorer__divider\[hidden\] \{[\s\S]*?display:\s*none;/.test(css),
  );
  check(
    'fold hide: --tree-collapsed hides the tree and divider',
    /\.file-explorer__body--tree-collapsed \.file-explorer__tree-viewport,\s*\n\.file-explorer__body--tree-collapsed \.file-explorer__divider \{[\s\S]*?display:\s*none;/.test(css),
  );
  check(
    'code pane keeps a 20-line desktop floor and independent scroll',
    css.includes('min-height: calc(20 * 1.45em)')
      && /\.file-explorer__code\s*\{[^}]*overflow-x:\s*auto/.test(css)
      && /\.file-explorer__code\s*\{[^}]*overflow-y:\s*auto/.test(css),
  );
  check(
    'logical lines never wrap and share one line height',
    /\.file-explorer__line\s*\{[^}]*white-space:\s*pre/.test(css)
      && /\.file-explorer__line\s*\{[^}]*align-items:\s*start/.test(css)
      && /\.file-explorer__line\s*\{[^}]*height:\s*1\.45em/.test(css)
      && !/\.file-explorer__line\s*\{[^}]*white-space:\s*pre-wrap/.test(css)
      && !/\.file-explorer__line\s*\{[^}]*overflow-wrap:\s*anywhere/.test(css),
  );
  check(
    'line numbers are sticky-left with opaque background',
    /\.file-explorer__line-no\s*\{[^}]*position:\s*sticky/.test(css)
      && /\.file-explorer__line-no\s*\{[^}]*left:\s*0/.test(css)
      && /\.file-explorer__line-no\s*\{[^}]*background:\s*var\(--bg-deep\)/.test(css),
  );
  check(
    'diff rows reuse the no-wrap line class',
    panel.includes('className={`file-explorer__line file-explorer__line--${row.kind}')
      && /\.file-explorer__line--addition/.test(css)
      && /\.file-explorer__line--deletion/.test(css)
      && /\.file-explorer__line--meta/.test(css),
  );
  check(
    'markdown viewport shares independent scroll and 20-line floor',
    /\.file-explorer__markdown\s*\{[^}]*overflow-x:\s*auto/.test(css)
      && /\.file-explorer__markdown\s*\{[^}]*overflow-y:\s*auto/.test(css)
      && /\.file-explorer__markdown\s*\{[^}]*min-height:\s*calc\(20 \* 1\.45em\)/.test(css)
      && /\.file-explorer__markdown\s*\{[^}]*font-family:\s*var\(--font-prose\)/.test(css),
  );
  check(
    'panel highlights loaded window via highlightDiffLineFragments',
    panel.includes('highlightDiffLineFragments')
      && panel.includes('diffPathLanguage(fileState.path)')
      && panel.includes('dangerouslySetInnerHTML={{ __html: fragment }}'),
  );
  check(
    'markdown files render through MarkdownBody without line numbers',
    panel.includes('fileExplorerPreviewKind')
      && panel.includes('<MarkdownBody')
      && panel.includes('file-explorer__markdown'),
  );
  check(
    'preview toggle lives on the content toolbar, not the Files header',
    panel.includes('file-explorer__surface')
      && panel.includes("role=\"radiogroup\"")
      && panel.indexOf('file-explorer__body') < panel.indexOf('file-explorer__surface'),
  );
  check(
    'loaded/total footer is removed; internal paging stays',
    !panel.includes('file-explorer__footer')
      && !panel.includes('fileExplorerFooterLabel')
      && panel.includes('fileExplorerShouldLoadNext'),
  );
  check(
    'mobile status chip is a 44x44 touch target',
    cssDeclaresMinBox(css, '.file-explorer__status', 44),
  );
  check(
    'panel seeds page cache from the external store',
    panel.includes('useSyncExternalStore')
      && panel.includes('getFileExplorerPageCacheSnapshot')
      && panel.includes('updateFileExplorerPageCache'),
  );
}

// ---- behavioral: download settlement decision (stale vs user-owned) ----
{
  const transport = Object.assign(new Error('connection closed (code 1006)'), { transport: true });
  const business = new Error('file is unavailable');
  // Stale-session settlement is exact non-fatal suppression: zero rows.
  check('stale download settlement ignores the rejection', fileExplorerDownloadSettledError(false, transport) === null && fileExplorerDownloadSettledError(false, business) === null);
  // Rendered-session rejections are user-owned: transport included, one row each.
  check('current-session transport rejection surfaces the actionable row', fileExplorerDownloadSettledError(true, transport) === 'connection closed (code 1006)', String(fileExplorerDownloadSettledError(true, transport)));
  check('current-session business rejection surfaces the actionable row', fileExplorerDownloadSettledError(true, business) === 'file is unavailable');
  check('current-session non-error rejection surfaces the request-failed row', fileExplorerDownloadSettledError(true, null) === 'request failed');
  // Blank formatted errors never render a blank/invisible row: an
  // empty-message Error, an empty-string primitive, and a whitespace-only
  // message all fall to the fixed actionable message.
  check('current-session empty-message Error surfaces the fixed request-failed row',
    fileExplorerDownloadSettledError(true, new Error('')) === 'request failed');
  check('current-session empty-string rejection surfaces the fixed request-failed row',
    fileExplorerDownloadSettledError(true, '') === 'request failed');
  check('current-session whitespace-only message surfaces the fixed request-failed row',
    fileExplorerDownloadSettledError(true, new Error('   ')) === 'request failed');
  check('current-session empty-message Error is local-owned so the fixed row renders exactly once',
    markdownOpenRejectionOwnership(new Error('')) === 'local');
}

// ---- behavioral: App markdown rejection ownership ----
{
  const toastOwned = Object.assign(new Error('rpc failed'), { rpc: true, toastOwned: true });
  const rpcQuiet = Object.assign(new Error('file is unavailable'), { rpc: true });
  const transport = Object.assign(new Error('not connected'), { transport: true });
  const drained = new Error('connection closed (code 1006): runner restarted');
  const timeout = new Error('command file_explorer_read timed out after 10s');
  const plain = new Error('session switch pending');
  // Already-surfaced and transport-owned rejections: zero local toasts.
  check('toastOwned rejection takes no local toast', markdownOpenRejectionOwnership(toastOwned) === 'none');
  check('transport-family rejection takes no local toast (shared outage toast owns it)',
    markdownOpenRejectionOwnership(transport) === 'none'
      && markdownOpenRejectionOwnership(drained) === 'none');
  // Unexpected/user-owned rejections surface exactly once locally.
  check('quiet rpc business rejection takes exactly one local toast', markdownOpenRejectionOwnership(rpcQuiet) === 'local');
  check('bounded command timeout takes exactly one local toast', markdownOpenRejectionOwnership(timeout) === 'local');
  check('plain unexpected rejection takes exactly one local toast', markdownOpenRejectionOwnership(plain) === 'local');
  // Drained outage reasons under rpc=true (connect() replace drain shape):
  // still transport-owned — zero local toasts, no duplicate of the shared
  // outage toast, and isTransportError's rpc invariant is untouched.
  const drainedReplace = Object.assign(new Error('connection replaced'), { rpc: true, toastOwned: false });
  const drainedClose = Object.assign(new Error('connection closed (code 1006)'), { rpc: true, toastOwned: false });
  check('rpc-tagged drained transport-family reason takes no local toast',
    markdownOpenRejectionOwnership(drainedReplace) === 'none'
      && markdownOpenRejectionOwnership(drainedClose) === 'none');
  // 'bootstrap failed' drain (traced: rpc=true, toastOwned=false, reason
  // outside the transport family): reconnect banner owns recovery, the
  // probe failure itself surfaces exactly once locally.
  const bootstrapFailed = Object.assign(new Error('bootstrap failed'), { rpc: true, toastOwned: false });
  check('bootstrap-failed drain rejection takes exactly one local toast', markdownOpenRejectionOwnership(bootstrapFailed) === 'local');
  // Promise.reject(null/undefined): unexpected, exactly one local toast —
  // and the classifier itself must not throw on the null-shaped rejection.
  check('null rejection is local ownership', markdownOpenRejectionOwnership(null) === 'local');
  check('undefined rejection is local ownership', markdownOpenRejectionOwnership(undefined) === 'local');
}

// ---- behavioral: App markdown-open blank-error detail (never-blank toast) ----
{
  // A local-owned rejection always toasts exactly once
  // (markdownOpenRejectionOwnership above); the detail must never trim to
  // blank, or the visible toast is an empty "open file failed: " shell.
  check('empty-message Error renders the fixed request-failed detail',
    markdownOpenErrorDetail(new Error('')) === 'request failed');
  check('whitespace-only message renders the fixed request-failed detail',
    markdownOpenErrorDetail(new Error('  \t ')) === 'request failed');
  check('empty-string rejection renders the fixed request-failed detail',
    markdownOpenErrorDetail('') === 'request failed');
  check('null rejection renders the fixed request-failed detail',
    markdownOpenErrorDetail(null) === 'request failed');
  check('undefined rejection renders the fixed request-failed detail',
    markdownOpenErrorDetail(undefined) === 'request failed');
  check('populated Error message is preserved verbatim',
    markdownOpenErrorDetail(new Error('file is unavailable')) === 'file is unavailable');
  check('non-Error rejection keeps its String form',
    markdownOpenErrorDetail('missing file') === 'missing file');
}

// ---- behavioral: App markdown-open missing-file verdict + bounded path ----
{
  // The verdict is true ONLY for the final stable state: the post-refresh
  // retry ran and the exact-owner probe still answered `file_unavailable`.
  // A pre-retry code (retry affordance still armed), any other code, and
  // non-RPC rejections (transport, timeout) never read as "file is gone".
  const codedError = (code: string) => Object.assign(new Error('x'), { rpcCode: code });
  check('missing-file verdict requires the post-retry file_unavailable code',
    markdownOpenMissingFileVerdict(true, codedError('file_unavailable')) === true);
  check('pre-retry file_unavailable is not a missing-file verdict (retry may succeed)',
    markdownOpenMissingFileVerdict(false, codedError('file_unavailable')) === false);
  check('other rpc codes are never a missing-file verdict',
    markdownOpenMissingFileVerdict(true, codedError('file_line_too_large')) === false
      && markdownOpenMissingFileVerdict(false, codedError('file_line_too_large')) === false
      && markdownOpenMissingFileVerdict(true, codedError('file_symlink')) === false);
  check('transport and plain rejections are never a missing-file verdict',
    markdownOpenMissingFileVerdict(true, new Error('send failed: socket closed')) === false
      && markdownOpenMissingFileVerdict(true, null) === false
      && markdownOpenMissingFileVerdict(true, undefined) === false);
  // Bounded path text: short paths stay verbatim; deep references truncate
  // from the LEFT (keeping the filename) and never exceed the documented cap.
  check('short missing path renders verbatim',
    markdownOpenBoundedPathText('psy_node_common/src/realm/processor/consensus.rs')
      === 'psy_node_common/src/realm/processor/consensus.rs');
  const deep = `a/b/c/d/e/f/${'segment/'.repeat(30)}consensus.rs`;
  const bounded = markdownOpenBoundedPathText(deep);
  check('deep missing path truncates from the left keeping the filename',
    bounded.endsWith('consensus.rs') && bounded.startsWith('…')
      && bounded.length === MARKDOWN_OPEN_MISSING_PATH_MAX_CHARS + 1
      && bounded.length < deep.length);
  check('missing path text is trimmed and never blank-inflated',
    markdownOpenBoundedPathText('  src/lib.rs  ') === 'src/lib.rs');
  check('stable missing-file toast names the bounded path',
    markdownOpenMissingFileToast('src/lib.rs') === '文件已不存在：src/lib.rs'
    && markdownOpenMissingFileToast(deep) === `文件已不存在：${bounded}`);
}

// ---- behavioral: server echo identity for the unique-basename fallback ----
{
  // The server resolves a cited basename-only reference (`init.rs`) to the
  // unique workspace file and echoes the REAL relative path. The identity
  // rule accepts that echo ONLY for a single-segment request whose echo is
  // strictly workspace-relative, resolves to the deeper path, and every
  // later page of the established identity matches exactly — a homonym
  // echo can never satisfy a resolved request.
  check('exact canonical echo still matches',
    fileExplorerResponseMatchesRequest('src/lib.rs', 'src/lib.rs') === true
      && fileExplorerResolvedRequestPath('src/lib.rs', 'src/lib.rs') === 'src/lib.rs');
  check('relative echo for an absolute request still matches the request',
    fileExplorerResponseMatchesRequest('/workspace/src/lib.rs', 'src/lib.rs') === true
      && fileExplorerResolvedRequestPath('/workspace/src/lib.rs', 'src/lib.rs') === '/workspace/src/lib.rs');
  check('single-segment request resolves the deeper unique-basename echo',
    fileExplorerResolvedRequestPath(
      'init.rs',
      'psy_node_common/src/realm/processor/db/init.rs',
    ) === 'psy_node_common/src/realm/processor/db/init.rs');
  check('resolved deeper echo rejects traversal and drive prefixes',
    fileExplorerResolvedRequestPath('init.rs', '../outside/init.rs') === null
      && fileExplorerResolvedRequestPath('init.rs', 'C:/outside/init.rs') === null
      && fileExplorerResponseMatchesRequest('init.rs', '../outside/init.rs') === false);
  check('multi-segment request never accepts an unrelated deeper echo',
    fileExplorerResolvedRequestPath('realm/init.rs', 'db/realm/init.rs') === null
      && fileExplorerResponseMatchesRequest('src/lib.rs', 'other/src/lib.rs') === false);
  check('single-segment request never accepts a partial-suffix echo',
    fileExplorerResolvedRequestPath('init.rs', 'db/another.init.rs') === null);
  check('absolute echo never matches a relative request',
    fileExplorerResolvedRequestPath('init.rs', '/workspace/init.rs') === null);
  // ESTABLISHED page identity is strict — even single-segment: once page 1
  // is in, later responses must match exactly; the relaxed deeper-echo
  // acceptance belongs to the one-time probe only.
  check('established single-segment page rejects deeper homonym echo',
    fileExplorerResponseMatchesRequest('init.rs', 'other/init.rs') === false
      && fileExplorerResolvedRequestPath('init.rs', 'other/init.rs') !== null);
  check('established page identity still accepts exact echo',
    fileExplorerResponseMatchesRequest('init.rs', 'init.rs') === true);
  // Two-page homonym identity: once the probe resolved the drawer's path,
  // later page responses only satisfy the ESTABLISHED identity — the exact
  // resolved echo. A different homonym's echo and a bare-basename echo both
  // fail, so pagination can never mix homonym content.
  const resolved = 'psy_node_common/src/realm/processor/db/init.rs';
  check('page-2 of the resolved identity matches exactly',
    fileExplorerResponseMatchesRequest(resolved, resolved) === true
      && fileExplorerResolvedRequestPath(resolved, resolved) === resolved);
  check('homonym and basename echoes never satisfy the resolved identity',
    fileExplorerResponseMatchesRequest(resolved, 'psy_node_common/src/coordinator/processor/core/init.rs') === false
      && fileExplorerResponseMatchesRequest(resolved, 'init.rs') === false);
}

// ---- behavioral: real page-state transition under the strict matcher ----
{
  // Root-level page 1 establishes the single-segment identity 'init.rs';
  // the file then disappears and page 2's server-side basename fallback
  // resolves a deeper homonym. The relaxed matcher must not apply at page
  // time: the homonym response is rejected as a path mismatch and the
  // original content stays; only the exact echo still appends.
  const req = { sessionId: 'sess-a', path: 'init.rs', view: 'file' as const, generation: 1 };
  const page1 = fileExplorerApplyPage(fileExplorerBeginPage(null, req), req, {
    path: 'init.rs', lines: ['root'], nextCursor: 1, truncated: true, totalLines: 2,
  });
  check('root-level page 1 establishes single-segment identity',
    page1 !== null && page1.lines.join(',') === 'root' && page1.loading === false && page1.nextCursor === 1);
  const moreReq = { ...req, cursor: 1, generation: 2 };
  const homonymPage = fileExplorerApplyPage(fileExplorerBeginPage(page1, moreReq), moreReq, {
    path: 'psy_node_common/src/realm/processor/db/init.rs', lines: ['homonym'], truncated: false, totalLines: 2,
  });
  check('deeper homonym page 2 rejected, original content unchanged',
    homonymPage !== null && homonymPage.lines.join(',') === 'root'
      && homonymPage.error?.message === 'response path mismatch' && homonymPage.loading === false);
  const exact = fileExplorerApplyPage(fileExplorerBeginPage(page1, moreReq), moreReq, {
    path: 'init.rs', lines: ['more'], truncated: false, totalLines: 2,
  });
  check('exact-echo page 2 still appends the established identity',
    exact !== null && exact.lines.join(',') === 'root,more' && exact.error === null);
}


// ---- explicit caps: 8 tabs per session (TUI file_viewer MAX_OPEN_FILES
// parity); the ninth open evicts the oldest non-active tab ----
{
  check('tab cap matches TUI file viewer', FILE_EXPLORER_MAX_OPEN_TABS === 8);
  let patch = updateFileExplorerSlot({}, 'sess-cap', (slot) => applyOpenPath(slot, 'f1.ts'));
  for (let i = 2; i <= 8; i += 1) {
    patch = updateFileExplorerSlot(patch.slots, 'sess-cap', (slot) => applyOpenPath(slot, `f${i}.ts`));
  }
  const eight = readFileExplorerSlot(patch.slots, 'sess-cap');
  check('eight tabs fit the cap', eight.tabs.length === 8 && eight.activePath === 'f8.ts');
  check('within-cap open evicts nothing', patch.droppedPathsBySession['sess-cap'] === undefined);
  patch = updateFileExplorerSlot(patch.slots, 'sess-cap', (slot) => applyOpenPath(slot, 'f9.ts'));
  const nine = readFileExplorerSlot(patch.slots, 'sess-cap');
  check('ninth open keeps exactly 8 tabs', nine.tabs.length === 8);
  check('ninth open evicts the oldest tab', !nine.tabs.some((t) => t.path === 'f1.ts') && nine.tabs[0].path === 'f2.ts');
  check('ninth open reports the evicted identity', (patch.droppedPathsBySession['sess-cap'] ?? []).join(',') === 'f1.ts');
  check('ninth open preserves the active tab', nine.activePath === 'f9.ts' && nine.tabs.some((t) => t.path === 'f8.ts'));
  // A→B→A: reopening an existing tab never evicts anything.
  const orderBefore = nine.tabs.map((t) => t.path).join(',');
  patch = updateFileExplorerSlot(patch.slots, 'sess-cap', (slot) => applyOpenPath(slot, 'f2.ts'));
  const reopenedA = readFileExplorerSlot(patch.slots, 'sess-cap');
  check('A→B→A reopen keeps all tabs', reopenedA.tabs.length === 8 && reopenedA.tabs.map((t) => t.path).join(',') === orderBefore && reopenedA.activePath === 'f2.ts');
  check('A→B→A reopen drops nothing', (patch.droppedPathsBySession['sess-cap'] ?? []).length === 0);
  // Re-select f2 as the active tab, then overflow with a ninth open: the
  // new path takes the active slot (TUI admit parity) and the oldest
  // non-active tab (f2) evicts — the ACTIVE tab is never a victim.
  patch = updateFileExplorerSlot(patch.slots, 'sess-cap', (slot) => applySelectTab(slot, 'f2.ts'));
  check('selected tab is active', readFileExplorerSlot(patch.slots, 'sess-cap').activePath === 'f2.ts');
  patch = updateFileExplorerSlot(patch.slots, 'sess-cap', (slot) => applyOpenPath(slot, 'f10.ts'));
  const overflow = readFileExplorerSlot(patch.slots, 'sess-cap');
  check('overflow keeps exactly 8 tabs with its active', overflow.tabs.length === 8 && overflow.activePath === 'f10.ts');
  check('active tab never an eviction victim', overflow.tabs.some((t) => t.path === 'f10.ts'));
  check('oldest non-active tab evicted next', (patch.droppedPathsBySession['sess-cap'] ?? []).join(',') === 'f2.ts' && !overflow.tabs.some((t) => t.path === 'f2.ts'));
}

// ---- bounded session-slot map: move-to-end LRU, fifth session evicts the
// least-recently-updated slot and reports its tab identities ----
{
  let patch = updateFileExplorerSlot({}, 'A', (slot) => applyOpenPath(slot, 'a.ts'));
  for (const sid of ['B', 'C', 'D']) {
    patch = updateFileExplorerSlot(patch.slots, sid, (slot) => applyOpenPath(slot, `${sid.toLowerCase()}.ts`));
  }
  check('four session slots fit', Object.keys(patch.slots).length === 4 && patch.evictedSessions.length === 0);
  // Refresh A's recency, then insert E: B is the least-recently-updated.
  patch = updateFileExplorerSlot(patch.slots, 'A', (slot) => applyDrawerMode(slot, 'open'));
  patch = updateFileExplorerSlot(patch.slots, 'E', (slot) => applyOpenPath(slot, 'e.ts'));
  check('session slot map stays at cap', Object.keys(patch.slots).length === 4);
  check('least-recently-updated session evicted', patch.evictedSessions.join(',') === 'B');
  check('active session slots preserved', patch.slots['A'] !== undefined && patch.slots['E'] !== undefined);
  check('evicted session tabs fully reported', (patch.droppedPathsBySession['B'] ?? []).join(',') === 'b.ts');
}

// ---- close reports canonical dropped identity (cache key space) ----
{
  let patch = updateFileExplorerSlot({}, 'sess-canon', (slot) => applyOpenPath(slot, './x'));
  patch = updateFileExplorerSlot(patch.slots, 'sess-canon', (slot) => applyCloseTab(slot, './x'));
  check('close reports canonical dropped identity', (patch.droppedPathsBySession['sess-canon'] ?? []).join(',') === 'x');
}

// ---- registry eviction follows the slot map: admitted tabs keep file+diff
// keys; close / evict drops both; stale writes and late responses cannot
// resurrect dropped keys ----
{
  const sessionId = 'sess-sync';
  let patch = updateFileExplorerSlot({}, sessionId, (slot) => applyOpenPath(slot, 'src/one.ts'));
  const reqOne = { sessionId, path: 'src/one.ts', view: 'file' as const, generation: 1 };
  const reqOneDiff = { sessionId, path: 'src/one.ts', view: 'diff' as const, generation: 2 };
  let cache = emptyFileExplorerPageCache();
  cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(cache, reqOne), reqOne, { path: 'src/one.ts', lines: ['one'], truncated: false });
  cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(cache, reqOneDiff), reqOneDiff, { path: 'src/one.ts', lines: ['+one'], truncated: false });
  cache = fileExplorerApplyCachedPage(
    fileExplorerBeginCachedPage(cache, { sessionId, path: 'src/two.ts', view: 'file' as const, generation: 3 }),
    { sessionId, path: 'src/two.ts', view: 'file' as const, generation: 3 },
    { path: 'src/two.ts', lines: ['two'], truncated: false },
  );
  check('seeded file, diff, and sibling keys', Object.keys(cache).length === 3);
  fileExplorerSyncPageCacheWithSlots(patch.slots);
  writeModuleFileExplorerPageCache(cache);
  const registry = readModuleFileExplorerPageCache();
  check('admitted tab keeps the file key', fileExplorerReadCachedPage(registry, sessionId, 'src/one.ts', 'file') !== null);
  check('admitted tab keeps the diff key', fileExplorerReadCachedPage(registry, sessionId, 'src/one.ts', 'diff') !== null);
  check('non-tab path is not retained', fileExplorerReadCachedPage(registry, sessionId, 'src/two.ts', 'file') === null);
  // Close the tab: both views drop from the registry.
  patch = updateFileExplorerSlot(patch.slots, sessionId, (slot) => applyCloseTab(slot, 'src/one.ts'));
  check('close reports the dropped path', (patch.droppedPathsBySession[sessionId] ?? []).join(',') === 'src/one.ts');
  fileExplorerSyncPageCacheWithSlots(patch.slots);
  check('close drops the file key', fileExplorerReadCachedPage(readModuleFileExplorerPageCache(), sessionId, 'src/one.ts', 'file') === null);
  check('close drops the diff key', fileExplorerReadCachedPage(readModuleFileExplorerPageCache(), sessionId, 'src/one.ts', 'diff') === null);
  // A stale panel-state write-through cannot resurrect evicted keys, and
  // a late in-flight response for the evicted identity cannot either.
  writeModuleFileExplorerPageCache(cache);
  check('stale panel-state write keeps keys evicted', fileExplorerReadCachedPage(readModuleFileExplorerPageCache(), sessionId, 'src/one.ts', 'file') === null);
  const resurrect = fileExplorerApplyCachedPage(readModuleFileExplorerPageCache(), reqOne, { path: 'src/one.ts', lines: ['late'], truncated: false });
  writeModuleFileExplorerPageCache(resurrect);
  check('late response cannot resurrect an evicted key', fileExplorerReadCachedPage(readModuleFileExplorerPageCache(), sessionId, 'src/one.ts', 'file') === null);
  // Cleanup so later scenarios start from a disarmed-empty registry.
  fileExplorerSyncPageCacheWithSlots({});
  check('registry emptied after sync release', Object.keys(readModuleFileExplorerPageCache()).length === 0);
}

// ---- aggregate cache caps: entries (LRU), whole-session eviction,
// aggregate retained lines and UTF-16 byte estimate ----
{
  let cache = emptyFileExplorerPageCache();
  for (let i = 1; i <= 70; i += 1) {
    const req = { sessionId: 'sess-bnd', path: `p${i}.txt`, view: 'file' as const, generation: i };
    cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(cache, req), req, { path: `p${i}.txt`, lines: [`content-${i}`], truncated: false });
  }
  const est = fileExplorerPageCacheEstimate(cache);
  check('entry cap holds', est.entries === FILE_EXPLORER_MAX_CACHE_ENTRIES && est.entries === 64);
  check('LRU evicted the oldest entries', fileExplorerReadCachedPage(cache, 'sess-bnd', 'p1.txt', 'file') === null);
  check('recent entries survive', fileExplorerReadCachedPage(cache, 'sess-bnd', 'p70.txt', 'file') !== null);
}
{
  let cache = emptyFileExplorerPageCache();
  for (let i = 0; i < 5; i += 1) {
    const req = { sessionId: `s${i}`, path: 'x.ts', view: 'file' as const, generation: i + 1 };
    cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(cache, req), req, { path: 'x.ts', lines: ['x'], truncated: false });
  }
  const est = fileExplorerPageCacheEstimate(cache);
  check('session cap holds', est.sessions === FILE_EXPLORER_MAX_SESSIONS && est.sessions === 4);
  check('first session evicted whole', fileExplorerReadCachedPage(cache, 's0', 'x.ts', 'file') === null);
  check('newest session survives', fileExplorerReadCachedPage(cache, 's4', 'x.ts', 'file') !== null);
}
{
  const big = Array.from({ length: 1500 }, () => 'line-xx-xxxxxxxxxx');
  let cache = emptyFileExplorerPageCache();
  for (let i = 0; i < 40; i += 1) {
    const req = { sessionId: 'sess-lines', path: `big${i}.txt`, view: 'file' as const, generation: i + 1 };
    cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(cache, req), req, { path: `big${i}.txt`, lines: big, truncated: false });
  }
  const est = fileExplorerPageCacheEstimate(cache);
  check('aggregate line cap binds below the entry cap', est.lines <= FILE_EXPLORER_MAX_CACHE_LINES && est.entries < FILE_EXPLORER_MAX_CACHE_ENTRIES);
  check('aggregate byte estimate is bounded', est.bytes <= FILE_EXPLORER_MAX_CACHE_BYTES);
  check('byte estimate sums UTF-16 units', est.bytes === est.lines * (big[0].length + 1) * 2);
}

// ---- LRU touch refresh: A→B→A re-touch survives the cap, stale B evicts ----
{
  let cache = emptyFileExplorerPageCache();
  const reqA = { sessionId: 'sess-lru', path: 'a.txt', view: 'file' as const, generation: 1 };
  const reqB = { sessionId: 'sess-lru', path: 'b.txt', view: 'file' as const, generation: 2 };
  cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(cache, reqA), reqA, { path: 'a.txt', lines: ['A'], truncated: false });
  cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(cache, reqB), reqB, { path: 'b.txt', lines: ['B'], truncated: false });
  for (let i = 0; i < 62; i += 1) {
    const req = { sessionId: 'sess-lru', path: `q${i}.txt`, view: 'file' as const, generation: 10 + i };
    cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(cache, req), req, { path: `q${i}.txt`, lines: ['q'], truncated: false });
  }
  check('within cap nothing is evicted (64 entries)', Object.keys(cache).length === 64);
  const reqA2 = { ...reqA, generation: 3 };
  cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(cache, reqA2), reqA2, { path: 'a.txt', lines: ['A2'], truncated: false });
  for (let i = 62; i < 70; i += 1) {
    const req = { sessionId: 'sess-lru', path: `q${i}.txt`, view: 'file' as const, generation: 10 + i };
    cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(cache, req), req, { path: `q${i}.txt`, lines: ['q'], truncated: false });
  }
  check('re-touched A survives the cap', fileExplorerReadCachedPage(cache, 'sess-lru', 'a.txt', 'file')?.lines.join(',') === 'A2');
  check('untouched B is the LRU victim', fileExplorerReadCachedPage(cache, 'sess-lru', 'b.txt', 'file') === null);
}

// ---- changes truncated wire + workspace-relative display identity ----
{
  const capped = normalizeChanges({ files: [{ path: 'a.ts', status: 'modified' }], isRepo: true, truncated: true });
  check('changes wire exposes truncated', capped !== null && capped.truncated === true);
  const uncapped = normalizeChanges({ files: [], isRepo: true });
  check('changes wire defaults truncated false', uncapped !== null && uncapped.truncated === false);
  check('display path keeps relative identity', fileExplorerDisplayPath('src/a.ts') === 'src/a.ts');
  check('display path is workspace-relative under cwd', fileExplorerDisplayPath('/workspace/src/a.ts', '/workspace') === 'src/a.ts');
  check('display path collapses dot segments', fileExplorerDisplayPath('/workspace/./src/../a.ts', '/workspace') === 'a.ts');
  check('display path root becomes a dot', fileExplorerDisplayPath('/workspace', '/workspace') === '.');
  check('display path outside workspace is symbolic', fileExplorerDisplayPath('/elsewhere/a.ts', '/workspace') === '<workspace>/elsewhere/a.ts');
  check('display path absolute without cwd is symbolic', fileExplorerDisplayPath('/elsewhere/a.ts') === '<workspace>/elsewhere/a.ts');
}


// ---- P1 privacy/cap: bounded change rows + workspace-relative display ----
// SSR renders the initial state only (effects never run), so the bounded-row
// and truncated-copy wiring is proven at source level (same style as the
// rejected-chrome source assertions), while the absolute→relative display is
// proven behaviorally through the slot activePath + workspaceCwd props. The
// request/click identity staying raw while display surfaces go through
// fileExplorerDisplayPath is the owner-safe split.
{
  const panel = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/panels/FileExplorerPanel.tsx'), 'utf8');
  check(
    'panel mirrors the backend changes cap',
    panel.includes('const CHANGES_CAP = 5000') && panel.includes('changes.slice(0, CHANGES_CAP)'),
  );
  check(
    'merged tree uses the capped change slice, never an orphan list',
    panel.includes('visibleChanges') && panel.includes('fileExplorerMergeTree') && !panel.includes('orphanChanges'),
  );
  check(
    'truncated wire flag drives the visible cap status',
    panel.includes('setChangesTruncated(parsed.truncated)')
      && panel.includes('const showChangesTruncated = changesTruncated || changes.length > CHANGES_CAP'),
  );
  check(
    'truncated status copy is visible and names the cap',
    panel.includes('Changes truncated (5000-entry cap).'),
  );
  check(
    'display surfaces render through fileExplorerDisplayPath',
    panel.includes('safeText(fileExplorerDisplayPath(row.path, workspaceCwd))')
      && panel.includes('`Open diff for ${displayPath(change.path)}`')
      && panel.includes('const displayPath = (path: string) => fileExplorerDisplayPath(path, workspaceCwd)'),
  );
  check(
    'request and click identity stays raw while display is relative',
    panel.includes('onClick={disabledReason == null ? () => onOpenDiff(change.path) : undefined}')
      && panel.includes('else onOpenFile(row.path)')
      && panel.includes('onDeleteFile(row.path)')
      && panel.includes('fileExplorerReadCommand(tab.path, cursor, FILE_EXPLORER_DEFAULT_MAX_LINES)')
      && panel.includes('fileExplorerDiffCommand(tab.path, cursor, FILE_EXPLORER_DEFAULT_MAX_LINES)')
      && panel.includes('fileExplorerStageDownloadCommand(path)'),
  );
}

// ---- content viewport: markdown body + bounded highlight.js ----
// SSR seeds the module page cache so the first render includes loaded
// lines (effects never run). Markdown files skip line numbers and go
// through MarkdownBody; known code extensions highlight the loaded
// window only; unknown extensions stay escaped React text.
{
  check('markdown path helper matches .md and .markdown', fileExplorerIsMarkdownPath('docs/README.md') && fileExplorerIsMarkdownPath('notes.markdown'));
  check('markdown path helper rejects code extensions', !fileExplorerIsMarkdownPath('src/a.ts') && !fileExplorerIsMarkdownPath('src/a.rs'));

  const panelProps = {
    sendCommand: () => Promise.resolve({}),
    hostAuthority: '127.0.0.1:1',
    pageProtocol: 'http:',
    connectionEpoch: 0,
    onOpenPath: () => {},
    onClose: () => {},
  };

  const seedFile = (sessionId: string, path: string, lines: string[]) => {
    fileExplorerSyncPageCacheWithSlots({
      [sessionId]: { tabs: [{ path, view: 'file' as const }], activePath: path, mode: 'open' },
    });
    const request = { sessionId, path, view: 'file' as const, generation: 1 };
    let cache = emptyFileExplorerPageCache();
    cache = fileExplorerBeginCachedPage(cache, request);
    cache = fileExplorerApplyCachedPage(cache, request, { path, lines, truncated: false });
    writeModuleFileExplorerPageCache(cache);
  };

  seedFile('sess-md', 'docs/README.md', ['# Title', '', '**bold** and `code`']);
  const mdHtml = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-md',
    slot: { tabs: [{ path: 'docs/README.md', view: 'file' }], activePath: 'docs/README.md', mode: 'open' },
  }));
  check('markdown file uses MarkdownBody surface', mdHtml.includes('file-explorer__markdown') && mdHtml.includes('file-explorer__markdown-body'), mdHtml.slice(0, 400));
  check('markdown heading and inline markup render', mdHtml.includes('class="md-h1"') && mdHtml.includes('<strong>bold</strong>') && mdHtml.includes('<code class="md-code">code</code>'), mdHtml);
  check('markdown file omits line numbers', !mdHtml.includes('file-explorer__line-no') && !mdHtml.includes('file-explorer__code'), mdHtml);
  check('previewable surface sits in the content viewport', mdHtml.includes('file-explorer__surface') && /file-explorer__content[\s\S]*file-explorer__surface/.test(mdHtml) && mdHtml.indexOf('data-testid="file-explorer-header"') < mdHtml.indexOf('file-explorer__body') && !/data-testid="file-explorer-header"[\s\S]*file-explorer__surface[\s\S]*file-explorer__body/.test(mdHtml));

  check('preview kind classifies markdown html svg', fileExplorerPreviewKind('docs/README.md') === 'markdown' && fileExplorerPreviewKind('index.HTML') === 'html' && fileExplorerPreviewKind('icon.svg') === 'svg' && fileExplorerPreviewKind('src/a.ts') === null);
  const emptyView = emptyFileExplorerViewState();
  check('previewable file defaults to preview', fileExplorerContentSurface(emptyView, 'page.htm', 'file') === 'preview');
  check('diff view stays source', fileExplorerContentSurface(emptyView, 'page.html', 'diff') === 'source');
  check('source override sticks until preview is restored', fileExplorerContentSurface(fileExplorerWithPathSurface(emptyView, 'page.html', 'source'), 'page.html', 'file') === 'source' && fileExplorerContentSurface(fileExplorerWithPathSurface(fileExplorerWithPathSurface(emptyView, 'page.html', 'source'), 'page.html', 'preview'), 'page.html', 'file') === 'preview');

  seedFile('sess-html', 'page.html', ['<h1>Hi</h1>', '<script>alert(1)</script>']);
  const htmlHtml = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-html',
    slot: { tabs: [{ path: 'page.html', view: 'file' }], activePath: 'page.html', mode: 'open' },
  }));
  check('html preview uses a scriptless iframe', htmlHtml.includes('file-explorer__html-preview-frame') && htmlHtml.includes('sandbox=""') && htmlHtml.includes('referrerPolicy="no-referrer"'), htmlHtml.slice(0, 600));
  check('html preview iframe carries srcDoc', /srcdoc=/i.test(htmlHtml) && htmlHtml.includes('&lt;h1&gt;Hi&lt;/h1&gt;'), htmlHtml);

  writeModuleFileExplorerViewCache({
    'sess-html-src': fileExplorerWithPathSurface(emptyFileExplorerViewState(), 'page.html', 'source'),
  });
  seedFile('sess-html-src', 'page.html', ['<h1>Hi</h1>']);
  const htmlSrc = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-html-src',
    slot: { tabs: [{ path: 'page.html', view: 'file' }], activePath: 'page.html', mode: 'open' },
  }));
  check('html source surface uses line-numbered code', htmlSrc.includes('file-explorer__code') && htmlSrc.includes('file-explorer__line-no') && !htmlSrc.includes('file-explorer__html-preview-frame'), htmlSrc.slice(0, 500));
  writeModuleFileExplorerViewCache({});

  seedFile('sess-svg', 'icon.svg', ['<svg xmlns="http://www.w3.org/2000/svg"></svg>']);
  const svgHtml = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-svg',
    slot: { tabs: [{ path: 'icon.svg', view: 'file' }], activePath: 'icon.svg', mode: 'open' },
  }));
  check('svg preview uses a data-url image', svgHtml.includes('file-explorer__svg-preview') && svgHtml.includes('data:image/svg+xml'), svgHtml.slice(0, 500));

  seedFile('sess-rs', 'src/main.rs', ['fn main() {', '    println!("hi");', '}']);
  const rsHtml = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-rs',
    slot: { tabs: [{ path: 'src/main.rs', view: 'file' }], activePath: 'src/main.rs', mode: 'open' },
  }));
  check('rust file keeps sticky line numbers', rsHtml.includes('file-explorer__code') && rsHtml.includes('file-explorer__line-no') && rsHtml.includes('>1</span>'), rsHtml.slice(0, 500));
  check('rust file highlights via hljs spans', rsHtml.includes('hljs-keyword') || rsHtml.includes('class="hljs'), rsHtml);
  check('rust file does not render as markdown', !rsHtml.includes('file-explorer__markdown'), rsHtml.slice(0, 300));

  seedFile('sess-txt', 'notes.txt', ['plain line']);
  const txtHtml = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-txt',
    slot: { tabs: [{ path: 'notes.txt', view: 'file' }], activePath: 'notes.txt', mode: 'open' },
  }));
  check('unknown/plain file stays escaped React text', txtHtml.includes('file-explorer__line-text') && txtHtml.includes('plain line') && !txtHtml.includes('dangerouslySetInnerHTML'), txtHtml);

  const hostile = '<script>alert(1)</script>';
  seedFile('sess-md-x', 'docs/x.md', [hostile]);
  const mdHostile = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-md-x',
    slot: { tabs: [{ path: 'docs/x.md', view: 'file' }], activePath: 'docs/x.md', mode: 'open' },
  }));
  check('markdown hostile HTML stays literal', !mdHostile.includes('<script>') && mdHostile.includes('&lt;script&gt;'), mdHostile);

  seedFile('sess-rs-x', 'src/x.rs', [hostile]);
  const rsHostile = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-rs-x',
    slot: { tabs: [{ path: 'src/x.rs', view: 'file' }], activePath: 'src/x.rs', mode: 'open' },
  }));
  check('highlighted hostile HTML stays literal', !rsHostile.includes('<script>') && rsHostile.includes('&lt;script&gt;'), rsHostile);

  fileExplorerSyncPageCacheWithSlots({});
}

// ---- absolute tab identity is not content chrome; displayPath stays relative ----
{
  check(
    'in-workspace absolute display is workspace-relative',
    fileExplorerDisplayPath('/workspace/notes.txt', '/workspace') === 'notes.txt',
  );
  check(
    'out-of-workspace absolute display uses the symbolic prefix',
    fileExplorerDisplayPath('/secret/notes.txt') === '<workspace>/secret/notes.txt',
  );
  const sendCommand = () => Promise.resolve({});
  const absolutePath = '/workspace/notes.txt';
  const html = renderToStaticMarkup(createElement(FileExplorerPanel, {
    sendCommand,
    sessionId: 'sess-abs',
    slot: {
      tabs: [{ path: absolutePath, view: 'file' }],
      activePath: absolutePath,
      mode: 'open',
    },
    hostAuthority: '127.0.0.1:1',
    pageProtocol: 'http:',
    workspaceCwd: '/workspace',
    connectionEpoch: 0,
    onOpenPath: () => {},
    onClose: () => {},
  }));
  check('selected absolute path is not content-head chrome', !html.includes('file-explorer__content-path') && !html.includes('>notes.txt<'));
  const markupWithoutOwnerCwd = html.replace(/\sdata-workspace-cwd="\/workspace"/g, '');
  check(
    'raw absolute path is not a visible label',
    !markupWithoutOwnerCwd.includes(absolutePath) && !markupWithoutOwnerCwd.includes('/workspace'),
    html.slice(0, 300),
  );
}
{
  const sendCommand = () => Promise.resolve({});
  const absolutePath = '/secret/notes.txt';
  const html = renderToStaticMarkup(createElement(FileExplorerPanel, {
    sendCommand,
    sessionId: 'sess-abs-nocwd',
    slot: {
      tabs: [{ path: absolutePath, view: 'file' }],
      activePath: absolutePath,
      mode: 'open',
    },
    hostAuthority: '127.0.0.1:1',
    pageProtocol: 'http:',
    connectionEpoch: 0,
    onOpenPath: () => {},
    onClose: () => {},
  }));
  check('out-of-workspace selected path is not content chrome', !html.includes('file-explorer__content-path') && !html.includes('>/secret/notes.txt<'));
  check('raw absolute path without cwd is absent from the DOM', !html.includes('href="/secret/notes.txt"') && !html.includes('>/secret/notes.txt<'));
}

// ---- header: static Files text, drawer close is the only full close ----
{
  const html = renderToStaticMarkup(createElement(FileExplorerPanel, {
    sendCommand: () => Promise.resolve({}),
    sessionId: 'sess-close',
    slot: {
      tabs: [{ path: 'src/a.ts', view: 'file' }],
      activePath: 'src/a.ts',
      mode: 'open',
    },
    hostAuthority: '127.0.0.1:1',
    pageProtocol: 'http:',
    connectionEpoch: 0,
    onOpenPath: () => {},
    onClose: () => {},
  }));
  const headStart = html.indexOf('file-explorer__head');
  const bodyStart = html.indexOf('file-explorer__body');
  const head = headStart >= 0 && bodyStart > headStart ? html.slice(headStart, bodyStart) : '';
  const actionsOpen = head.indexOf('file-explorer__actions');
  const title = actionsOpen >= 0 ? head.slice(0, actionsOpen) : head;
  check('Files title is static text and is not a button', title.includes('>Files<') && !/>Files<\/button>/.test(title));
  check('title is not the close control', !html.includes('file-explorer-hide-btn') && !html.includes('Hide file explorer') && !html.includes('file-explorer__title-button'));
  check('drawer close is the registered control', head.includes('id="file-explorer-drawer-close-btn"') && head.includes('aria-label="Close file explorer"'));
  check('legacy overlay close id is absent', !html.includes('file-explorer-close-btn'));
  check('exactly one full close in the drawer', (html.split('id="file-explorer-drawer-close-btn"').length - 1) === 1);
}

// ---- cache bound: oversized single entry compacts, never evicts the
// written key ----
// A single entry that alone exceeds the aggregate line/byte caps cannot be
// evicted (that would drop the just-written key), so the bound compacts it
// to the last FILE_EXPLORER_MAX_LINES_PER_ENTRY lines, shifting startCursor
// so the loaded end stays coherent. The aggregate caps then hold by
// construction and the key survives.
{
  check('per-entry line budget equals the default page', FILE_EXPLORER_MAX_LINES_PER_ENTRY === FILE_EXPLORER_DEFAULT_MAX_LINES);
  const huge = Array.from({ length: 45000 }, (_, i) => `line-${i}-xxxxxxxxxxxxxxxxxxxx`);
  let cache = emptyFileExplorerPageCache();
  const req = { sessionId: 'sess-huge', path: 'huge.txt', view: 'file' as const, generation: 1 };
  cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(cache, req), req, { path: 'huge.txt', lines: huge, truncated: false, totalLines: huge.length });
  const est = fileExplorerPageCacheEstimate(cache);
  check('oversized single entry is compacted to the per-entry budget', est.lines === FILE_EXPLORER_MAX_LINES_PER_ENTRY, `lines=${est.lines}`);
  check('aggregate line cap holds after compaction', est.lines <= FILE_EXPLORER_MAX_CACHE_LINES);
  check('aggregate byte cap holds after compaction', est.bytes <= FILE_EXPLORER_MAX_CACHE_BYTES);
  const kept = fileExplorerReadCachedPage(cache, 'sess-huge', 'huge.txt', 'file');
  check('written key survives the bound', kept !== null);
  check('compaction keeps the LAST lines (loaded end coherent)', kept !== null && kept.lines[kept.lines.length - 1] === huge[huge.length - 1] && kept.lines[0] === huge[huge.length - FILE_EXPLORER_MAX_LINES_PER_ENTRY]);
  check('compacted entry covers a tail cursor (no refetch)', kept !== null && !fileExplorerShouldFetchPage(kept, { sessionId: 'sess-huge', path: 'huge.txt', view: 'file', cursor: huge.length - 1 }));
  check('compacted entry refetches a cursor outside the kept window', kept !== null && fileExplorerShouldFetchPage(kept, { sessionId: 'sess-huge', path: 'huge.txt', view: 'file', cursor: 0 }));
}

// The just-written key is passed as the bound's protected key: it is
// proactively compacted to the per-entry line budget (never evicted), and
// the eviction loop skips it — even when it is the LRU — so smaller
// entries take the eviction hits first.
{
  let cache = emptyFileExplorerPageCache();
  const big = Array.from({ length: 45000 }, (_, i) => `big-${i}-xxxxxxxxxxxxxxxxxxxx`);
  for (let i = 0; i < 70; i += 1) {
    const req = { sessionId: 'sess-mix', path: `small${i}.txt`, view: 'file' as const, generation: 1 + i };
    cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(cache, req), req, { path: `small${i}.txt`, lines: ['s'], truncated: false });
  }
  // The huge entry is written LAST, so it is the most-recently-touched key:
  // the bound evicts the older smalls first and compacts the written key
  // proactively — it is never dropped.
  const reqBig = { sessionId: 'sess-mix', path: 'big.txt', view: 'file' as const, generation: 100 };
  cache = fileExplorerApplyCachedPage(fileExplorerBeginCachedPage(cache, reqBig), reqBig, { path: 'big.txt', lines: big, truncated: false, totalLines: big.length });
  const est = fileExplorerPageCacheEstimate(cache);
  check('mixed cache stays within the aggregate line cap', est.lines <= FILE_EXPLORER_MAX_CACHE_LINES);
  check('mixed cache stays within the aggregate byte cap', est.bytes <= FILE_EXPLORER_MAX_CACHE_BYTES);
  check('mixed cache stays within the entry cap', est.entries <= FILE_EXPLORER_MAX_CACHE_ENTRIES);
  const keptBig = fileExplorerReadCachedPage(cache, 'sess-mix', 'big.txt', 'file');
  check('oversized written key survives among many entries', keptBig !== null);
  check('oversized entry is compacted to the per-entry budget', keptBig !== null && keptBig.lines.length === FILE_EXPLORER_MAX_LINES_PER_ENTRY);
  check('compacted window keeps the loaded end coherent', keptBig !== null && fileExplorerLoadedEnd(keptBig) === big.length);
  check('older small entries evicted first', fileExplorerReadCachedPage(cache, 'sess-mix', 'small0.txt', 'file') === null);
}

// entry instead — the written key is never a victim. (A scrollTop write on
// an old cached page reuses its original touch, so the protected key can
// genuinely be the LRU.)
{
  const big = Array.from({ length: 45000 }, (_, i) => `old-${i}-xxxxxxxxxxxxxxxxxxxx`);
  const stateOf = (path: string, lines: string[], touch: number, generation: number): FileExplorerPageState => ({
    kind: 'file',
    path,
    startCursor: 0,
    lines,
    totalLines: lines.length,
    truncated: false,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation,
    sessionId: 'sess-lru-protect',
    touch,
  });
  const cache: FileExplorerPageCache = {};
  const keyA = fileExplorerPageCacheKey('sess-lru-protect', 'a.txt', 'file');
  cache[keyA] = stateOf('a.txt', big, 1, 1);
  for (let i = 0; i < 64; i += 1) {
    const key = fileExplorerPageCacheKey('sess-lru-protect', `n${i}.txt`, 'file');
    cache[key] = stateOf(`n${i}.txt`, ['n'], 2 + i, 2 + i);
  }
  // 65 entries exceed the entry cap and A alone exceeds the line cap; A is
  // the LRU. Bounding with A protected must compact A and evict smalls.
  const bounded = fileExplorerBoundPageCache(cache, keyA);
  const est = fileExplorerPageCacheEstimate(bounded);
  check('LRU-protected cache stays within the aggregate line cap', est.lines <= FILE_EXPLORER_MAX_CACHE_LINES);
  check('LRU-protected cache stays within the aggregate byte cap', est.bytes <= FILE_EXPLORER_MAX_CACHE_BYTES);
  check('LRU-protected cache stays within the entry cap', est.entries <= FILE_EXPLORER_MAX_CACHE_ENTRIES);
  const keptA = bounded[keyA];
  check('oldest oversized written key survives (protected, not LRU-victim)', keptA !== null);
  check('oldest protected key is compacted to the per-entry budget', keptA !== null && keptA.lines.length === FILE_EXPLORER_MAX_LINES_PER_ENTRY);
  check('compacted oldest key keeps the loaded end coherent', keptA !== null && fileExplorerLoadedEnd(keptA) === big.length);
  check('newer entries evicted instead of the protected key', bounded[fileExplorerPageCacheKey('sess-lru-protect', 'n0.txt', 'file')] === undefined);
}

// ---- infinite scroll: 160px threshold, one in-flight, exhausted, one-page cursor ----
{
  check('shared next-page threshold is 160px', FILE_EXPLORER_NEXT_PAGE_NEAR_BOTTOM_PX === 160);
  const near = {
    scrollTop: 840,
    clientHeight: 400,
    scrollHeight: 1400,
    hasNext: true,
    isLoading: false,
  };
  check('near-bottom remaining 160px loads next', fileExplorerShouldLoadNext(near) === true);
  check('one pixel above the threshold is a no-op', fileExplorerShouldLoadNext({ ...near, scrollTop: 839 }) === false);
  check('in-flight never loads next', fileExplorerShouldLoadNext({ ...near, isLoading: true }) === false);
  check('exhausted cursor never loads next', fileExplorerShouldLoadNext({ ...near, hasNext: false }) === false);
  check('short viewport with no remaining scroll loads one page', fileExplorerShouldLoadNext({
    scrollTop: 0,
    clientHeight: 400,
    scrollHeight: 400,
    hasNext: true,
    isLoading: false,
  }) === true);
  check('non-finite geometry never loads next', fileExplorerShouldLoadNext({
    scrollTop: Number.NaN,
    clientHeight: 400,
    scrollHeight: 800,
    hasNext: true,
    isLoading: false,
  }) === false);
  const pageState: FileExplorerPageState = {
    kind: 'file',
    path: 'long.md',
    startCursor: 0,
    lines: Array.from({ length: 400 }, (_, i) => `L${i}`),
    nextCursor: 400,
    totalLines: 800,
    truncated: true,
    loading: false,
    error: null,
    transportIncomplete: false,
    generation: 1,
    sessionId: 'sess-page',
    touch: 1,
  };
  check('settled page with nextCursor can load more', fileExplorerCanLoadMore(pageState) === true);
  check('in-flight page does not load more', fileExplorerCanLoadMore({ ...pageState, loading: true }) === false);
  check('exhausted page does not load more', fileExplorerCanLoadMore({ ...pageState, nextCursor: undefined, totalLines: 400 }) === false);
}
await (async () => {
  const live = new Map();
  const aKey = fileExplorerInflightKey('sess-a', 'long.md', 'file', 0);
  const bKey = fileExplorerInflightKey('sess-b', 'long.md', 'file', 0);
  let aCreates = 0;
  let bCreates = 0;
  const pendingA = Promise.withResolvers<FileExplorerPageCache>();
  const pendingB = Promise.withResolvers<FileExplorerPageCache>();
  const requestA = { sessionId: 'sess-a', path: 'long.md', view: 'file' as const, generation: 1 };
  const requestB = { sessionId: 'sess-b', path: 'long.md', view: 'file' as const, generation: 1 };
  const firstA = fileExplorerAcquireInflight(aKey, () => {
    aCreates += 1;
    return { request: requestA, promise: pendingA.promise };
  }, live);
  check('A pending acquire creates one promise', aCreates === 1 && firstA.identity === aKey);
  const closeReopenA = fileExplorerAcquireInflight(aKey, () => {
    aCreates += 1;
    return { request: requestA, promise: pendingA.promise };
  }, live);
  check('mount-close-reopen reuses the pending A promise', closeReopenA.promise === firstA.promise && aCreates === 1);
  const strictA = fileExplorerAcquireInflight(aKey, () => {
    aCreates += 1;
    return { request: requestA, promise: pendingA.promise };
  }, live);
  check('StrictMode replay reuses the same A promise', strictA.promise === firstA.promise && aCreates === 1);
  const firstB = fileExplorerAcquireInflight(bKey, () => {
    bCreates += 1;
    return { request: requestB, promise: pendingB.promise };
  }, live);
  check('A and B coexist as distinct promises', firstB.promise !== firstA.promise && bCreates === 1 && fileExplorerPeekInflight(aKey, live) != null && fileExplorerPeekInflight(bKey, live) != null);
  const empty: FileExplorerPageCache = {};
  pendingA.resolve(empty);
  await firstA.promise;
  fileExplorerReleaseInflight(aKey, live);
  check('exact A settle drops only A', fileExplorerPeekInflight(aKey, live) == null && fileExplorerPeekInflight(bKey, live) != null);
  pendingB.resolve(empty);
  await firstB.promise;
  fileExplorerReleaseInflight(bKey, live);
  check('exact B settle empties the live map', fileExplorerPeekInflight(bKey, live) == null);
  fileExplorerSyncPageCacheWithSlots({
    'sess-a': { tabs: [{ path: 'long.md', view: 'file' }], activePath: 'long.md', mode: 'open' },
    'sess-b': { tabs: [{ path: 'long.md', view: 'file' }], activePath: 'long.md', mode: 'open' },
  });
  writeModuleFileExplorerPageCache(fileExplorerBeginCachedPage(emptyFileExplorerPageCache(), requestA));
  writeModuleFileExplorerPageCache(fileExplorerBeginCachedPage(readModuleFileExplorerPageCache(), requestB));
  const settled = fileExplorerSettleInflightCache(requestA, { path: 'long.md', lines: ['ok'], truncated: false, totalLines: 1 });
  check('shared settle writes the module page cache', fileExplorerReadCachedPage(settled, 'sess-a', 'long.md', 'file')?.lines[0] === 'ok');
  const failed = fileExplorerSettleInflightCache(requestB, null, new Error('boom'));
  check('transport-independent settle records the error once', fileExplorerReadCachedPage(failed, 'sess-b', 'long.md', 'file')?.error?.message === 'boom' && fileExplorerReadCachedPage(failed, 'sess-b', 'long.md', 'file')?.error?.code === undefined);
  writeModuleFileExplorerPageCache(fileExplorerBeginCachedPage(readModuleFileExplorerPageCache(), requestB));
  const codedErr = Object.assign(new Error('line exceeds response limit'), { rpcCode: 'file_line_too_large' });
  const codedSettle = fileExplorerSettleInflightCache(requestB, null, codedErr);
  const codedPage = fileExplorerReadCachedPage(codedSettle, 'sess-b', 'long.md', 'file');
  check('host-stamped known rpcCode is retained', codedPage?.error?.message === 'line exceeds response limit' && codedPage?.error?.code === 'file_line_too_large');
  writeModuleFileExplorerPageCache(fileExplorerBeginCachedPage(readModuleFileExplorerPageCache(), requestB));
  const unknownSettle = fileExplorerSettleInflightCache(requestB, null, Object.assign(new Error('future failure'), { rpcCode: 'future_code' }));
  check('unknown rpcCode is dropped', fileExplorerReadCachedPage(unknownSettle, 'sess-b', 'long.md', 'file')?.error?.message === 'future failure' && fileExplorerReadCachedPage(unknownSettle, 'sess-b', 'long.md', 'file')?.error?.code === undefined);
  writeModuleFileExplorerPageCache(fileExplorerBeginCachedPage(readModuleFileExplorerPageCache(), requestB));
  const malformedSettle = fileExplorerSettleInflightCache(requestB, null, Object.assign(new Error('malformed failure'), { rpcCode: 7 }));
  check('malformed rpcCode is dropped', fileExplorerReadCachedPage(malformedSettle, 'sess-b', 'long.md', 'file')?.error?.message === 'malformed failure' && fileExplorerReadCachedPage(malformedSettle, 'sess-b', 'long.md', 'file')?.error?.code === undefined);
  const newer = {
    sessionId: 'sess-a',
    path: 'long.md',
    view: 'file' as const,
    cursor: 400,
    generation: 2,
  };
  writeModuleFileExplorerPageCache(fileExplorerBeginCachedPage(readModuleFileExplorerPageCache(), newer));
  const newerBefore = fileExplorerReadCachedPage(readModuleFileExplorerPageCache(), 'sess-a', 'long.md', 'file');
  const late = fileExplorerSettleInflightCache(requestA, { path: 'long.md', lines: ['stale-old'], truncated: false, totalLines: 1 });
  const afterLate = fileExplorerReadCachedPage(late, 'sess-a', 'long.md', 'file');
  check('old cursor settle does not rewind a newer generation', afterLate != null && newerBefore != null && afterLate.generation === 2 && afterLate.lines[0] !== 'stale-old' && afterLate.loading === true);
  const oldGen = nextFileExplorerRequestGeneration();
  const reopenGen = nextFileExplorerRequestGeneration();
  const ownerB = nextFileExplorerRequestGeneration();
  check('global generation is strictly increasing', oldGen >= 1 && reopenGen === oldGen + 1 && ownerB === reopenGen + 1);
  check('peek matches the last allocated generation', fileExplorerPeekRequestGeneration() === ownerB);
  const oldRequest = { sessionId: 'sess-a', path: 'long.md', view: 'file' as const, cursor: 0, generation: oldGen };
  const newRequest = { sessionId: 'sess-a', path: 'long.md', view: 'file' as const, cursor: 400, generation: reopenGen };
  writeModuleFileExplorerPageCache(fileExplorerBeginCachedPage(readModuleFileExplorerPageCache(), newRequest));
  const afterReopen = fileExplorerReadCachedPage(readModuleFileExplorerPageCache(), 'sess-a', 'long.md', 'file');
  const lateOld = fileExplorerSettleInflightCache(oldRequest, { path: 'long.md', lines: ['old-gen1'], truncated: false, totalLines: 1 });
  const afterOld = fileExplorerReadCachedPage(lateOld, 'sess-a', 'long.md', 'file');
  check('old gen settle after reopen different cursor is a no-op', afterOld != null && afterReopen != null && afterOld.generation === reopenGen && afterOld.lines[0] !== 'old-gen1');
})();

{
  fileExplorerSyncPageCacheWithSlots({
    'sess-a': { tabs: [{ path: 'a.ts', view: 'file' }], activePath: 'a.ts', mode: 'open' },
    'sess-b': { tabs: [{ path: 'b.ts', view: 'file' }], activePath: 'b.ts', mode: 'open' },
  });
  const reqA = { sessionId: 'sess-a', path: 'a.ts', view: 'file' as const, generation: nextFileExplorerRequestGeneration() };
  const reqB = { sessionId: 'sess-b', path: 'b.ts', view: 'file' as const, generation: nextFileExplorerRequestGeneration() };
  writeModuleFileExplorerPageCache(fileExplorerBeginCachedPage(emptyFileExplorerPageCache(), reqA));
  writeModuleFileExplorerPageCache(fileExplorerBeginCachedPage(readModuleFileExplorerPageCache(), reqB));
  fileExplorerSettleInflightCache(reqB, { path: 'b.ts', lines: ['B'], truncated: false, totalLines: 1 });
  fileExplorerSettleInflightCache(reqA, { path: 'a.ts', lines: ['A'], truncated: false, totalLines: 1 });
  writeModuleFileExplorerPageCache(fileExplorerSetCachedScrollTop(readModuleFileExplorerPageCache(), 'sess-b', 'b.ts', 'file', 40));
  const afterScroll = readModuleFileExplorerPageCache();
  check('B scroll retains A settlement', fileExplorerReadCachedPage(afterScroll, 'sess-a', 'a.ts', 'file')?.lines[0] === 'A');
  check('B scroll updates only B', fileExplorerReadCachedPage(afterScroll, 'sess-b', 'b.ts', 'file')?.scrollTop === 40);
  fileExplorerSyncPageCacheWithSlots({
    'sess-b': { tabs: [{ path: 'b.ts', view: 'file' }], activePath: 'b.ts', mode: 'open' },
  });
  const liveGen = nextFileExplorerRequestGeneration();
  const liveReq = { sessionId: 'sess-b', path: 'b.ts', view: 'file' as const, cursor: 0, generation: liveGen };
  writeModuleFileExplorerPageCache(fileExplorerBeginCachedPage(readModuleFileExplorerPageCache(), liveReq));
  fileExplorerSyncPageCacheWithSlots({});
  const reopenGen = nextFileExplorerRequestGeneration();
  const reopenReq = { sessionId: 'sess-b', path: 'b.ts', view: 'file' as const, cursor: 400, generation: reopenGen };
  fileExplorerSyncPageCacheWithSlots({
    'sess-b': { tabs: [{ path: 'b.ts', view: 'file' }], activePath: 'b.ts', mode: 'open' },
  });
  writeModuleFileExplorerPageCache(fileExplorerBeginCachedPage(readModuleFileExplorerPageCache(), reopenReq));
  const afterEvict = fileExplorerSettleInflightCache(liveReq, { path: 'b.ts', lines: ['stale-live'], truncated: false, totalLines: 1 });
  check('evicted live request cannot clobber a newer unique generation', fileExplorerReadCachedPage(afterEvict, 'sess-b', 'b.ts', 'file')?.generation === reopenGen);
}

// ---- one Git-aware merged tree: inject deleted/renamed, aggregate dirs ----
{
  const entries = [
    { name: 'src', path: 'src', kind: 'dir' as const },
    { name: 'keep.ts', path: 'keep.ts', kind: 'file' as const, sizeBytes: 4 },
  ];
  const changes = [
    { path: 'keep.ts', status: 'modified' as const },
    { path: 'gone.txt', status: 'deleted' as const },
    { path: 'nested/renamed.txt', oldPath: 'nested/old.txt', status: 'renamed' as const },
    { path: 'src/a.ts', status: 'added' as const },
    { path: 'missing-mod.ts', status: 'modified' as const },
  ];
  const root = fileExplorerMergeTree('', entries, changes);
  check('root has no orphan list', root.every((row) => row.path !== 'Changed files'));
  check('modified listing file keeps its status', root.some((row) => row.path === 'keep.ts' && row.change?.status === 'modified' && !row.gitOnly));
  check('deleted path injects as gitOnly at root', root.some((row) => row.path === 'gone.txt' && row.gitOnly === true && row.kind === 'file' && row.change?.status === 'deleted'));
  check('absent modified stays ordinary/openable', root.some((row) => row.path === 'missing-mod.ts' && !row.gitOnly && row.change?.status === 'modified'));
  const nestedDir = root.find((row) => row.path === 'nested');
  check('one logical rename aggregates once', nestedDir != null && nestedDir.kind === 'dir' && nestedDir.changeCount === 1 && nestedDir.changeTone === 'renamed');
  const nested = fileExplorerMergeTree('nested', [], changes);
  const renamedNew = nested.find((row) => row.path === 'nested/renamed.txt');
  const renamedOld = nested.find((row) => row.path === 'nested/old.txt');
  check('renamed new path injects as ordinary/openable', renamedNew != null && renamedNew.gitOnly !== true && renamedNew.change?.status === 'renamed' && renamedNew.change.path === 'nested/renamed.txt');
  check('renamed old path injects as gitOnly and diffs the new identity', renamedOld != null && renamedOld.gitOnly === true && renamedOld.change?.path === 'nested/renamed.txt');
  const src = root.find((row) => row.path === 'src');
  check('directory rows aggregate descendant change count/tone', src != null && src.changeCount === 1 && src.changeTone === 'added');
  const srcAgg = fileExplorerDirAggregate('src', changes);
  check('dir aggregate counts descendants only', srcAgg.count === 1 && srcAgg.tone === 'added');
  const rootAgg = fileExplorerDirAggregate('', changes);
  check('root aggregate counts each logical change once', rootAgg.count === 5);
  const deletedNested = fileExplorerMergeTree('', [], [
    { path: 'deployments/sepolia/deploy.json', status: 'deleted' },
  ]);
  check(
    'deleted nested tree hoists the catalog file and drops virtual dirs',
    deletedNested.some((row) => row.path === 'deployments/sepolia/deploy.json' && row.kind === 'file' && row.gitOnly === true && row.name === 'deployments/sepolia/deploy.json' && row.change?.status === 'deleted')
      && !deletedNested.some((row) => row.kind === 'dir'),
  );
  const unavailableParent = fileExplorerMergeTree('', [
    { name: 'deployments', path: 'deployments', kind: 'dir' },
  ], [
    { path: 'deployments/sepolia/deploy.json', status: 'deleted' },
    { path: 'deployments/live.ts', status: 'modified' },
    { path: 'deployments/old.json', oldPath: 'deployments/renamed.json', status: 'renamed' },
  ], fileExplorerBuildChangeIndex([
    { path: 'deployments/sepolia/deploy.json', status: 'deleted' },
    { path: 'deployments/live.ts', status: 'modified' },
    { path: 'deployments/old.json', oldPath: 'deployments/renamed.json', status: 'renamed' },
  ]), { deployments: true });
  const hoistedDeleted = unavailableParent.filter((row) => row.path === 'deployments/sepolia/deploy.json');
  const hoistedRenamed = unavailableParent.filter((row) => row.path === 'deployments/renamed.json');
  check(
    'unavailable parent hoists deleted and rename-old files exactly once',
    hoistedDeleted.length === 1 && hoistedDeleted[0].gitOnly === true && hoistedDeleted[0].name === 'deployments/sepolia/deploy.json'
      && hoistedRenamed.length === 1 && hoistedRenamed[0].gitOnly === true && hoistedRenamed[0].name === 'deployments/renamed.json'
      && !unavailableParent.some((row) => row.kind === 'dir' || row.path === 'deployments/live.ts'),
  );
  const mixed = fileExplorerMergeTree('', [
    { name: 'src', path: 'src', kind: 'dir' },
  ], [
    { path: 'src/live.ts', status: 'modified' },
    { path: 'src/gone.ts', status: 'deleted' },
  ]);
  check(
    'a real directory with a deleted child stays expandable',
    mixed.some((row) => row.path === 'src' && row.kind === 'dir' && row.gitOnly !== true),
  );
  const duplicates = fileExplorerMergeTree('', [], [
    { path: 'a/config.json', status: 'deleted' },
    { path: 'b/config.json', status: 'deleted' },
  ]);
  check(
    'hoisted duplicate basenames keep distinct relative labels and paths',
    duplicates.filter((row) => row.name === 'a/config.json' && row.path === 'a/config.json').length === 1
      && duplicates.filter((row) => row.name === 'b/config.json' && row.path === 'b/config.json').length === 1,
  );
  const twoDeleted = fileExplorerMergeTree('', [], [
    { path: 'a/x.txt', status: 'deleted' },
    { path: 'a/y.txt', status: 'deleted' },
  ]);
  check(
    'two deleted files under one missing directory hoist once with no virtual dir',
    twoDeleted.filter((row) => row.path === 'a/x.txt' && row.gitOnly === true).length === 1
      && twoDeleted.filter((row) => row.path === 'a/y.txt' && row.gitOnly === true).length === 1
      && !twoDeleted.some((row) => row.kind === 'dir'),
  );
  const unlistedMixed = fileExplorerMergeTree('', [], [
    { path: 'a/live.ts', status: 'modified' },
    { path: 'a/gone.ts', status: 'deleted' },
  ]);
  check(
    'unlisted mixed catalog keeps the directory and does not hoist the deleted file',
    unlistedMixed.filter((row) => row.path === 'a' && row.kind === 'dir').length === 1
      && unlistedMixed.filter((row) => row.path === 'a/gone.ts').length === 0,
  );
}
{
  const listed = fileExplorerMergeTree('', [
    { name: 'ext', path: 'ext', kind: 'dir' },
    { name: 'keep.ts', path: 'keep.ts', kind: 'file' },
  ], [
    { path: 'ext', status: 'modified' },
    { path: 'ext/lib.txt', status: 'modified' },
  ]);
  const ext = listed.find((row) => row.path === 'ext');
  check('gitlink directory keeps dir kind', ext != null && ext.kind === 'dir');
  check('gitlink directory carries its own status letter', ext?.change?.status === 'modified' && ext?.changeTone === 'modified' && (ext?.changeCount ?? 0) >= 1);
  const inner = fileExplorerMergeTree('ext', [
    { name: 'lib.txt', path: 'ext/lib.txt', kind: 'file' },
    { name: 'src', path: 'ext/src', kind: 'dir' },
  ], [{ path: 'ext/lib.txt', status: 'modified' }]);
  check('submodule listing shows inner files with status', inner.some((row) => row.path === 'ext/lib.txt' && row.kind === 'file' && row.change?.status === 'modified'));
  check('submodule listing shows inner directories', inner.some((row) => row.path === 'ext/src' && row.kind === 'dir'));
}
{
  check('file explorer status letters reuse code-review glyphs', fileExplorerStatusLetter('added') === 'A' && fileExplorerStatusLetter('deleted') === 'D' && fileExplorerStatusLetter('modified') === 'M' && fileExplorerStatusLetter('renamed') === 'R');
}
// ---- preview size boundary + unified diff rows ----
{
  check('preview limit remains enabled exactly at 256 KiB', !fileExplorerIsOversized(FILE_EXPLORER_MAX_PREVIEW_BYTES));
  check('preview becomes oversized above 256 KiB', fileExplorerIsOversized(FILE_EXPLORER_MAX_PREVIEW_BYTES + 1));
  const rows = fileExplorerMergeTree('', [
    { name: 'limit.txt', path: 'limit.txt', kind: 'file', sizeBytes: FILE_EXPLORER_MAX_PREVIEW_BYTES },
    { name: 'large.txt', path: 'large.txt', kind: 'file', sizeBytes: FILE_EXPLORER_MAX_PREVIEW_BYTES + 1 },
  ], [{ path: 'large.txt', status: 'modified' }]);
  check('merged tree marks only above-limit filesystem file oversized', rows.find((row) => row.path === 'limit.txt')?.oversized !== true && rows.find((row) => row.path === 'large.txt')?.oversized === true);

  const diff = parseFileExplorerUnifiedDiff([
    'diff --git a/a.ts b/a.ts',
    '--- a/a.ts',
    '+++ b/a.ts',
    '@@ -10,3 +20,4 @@ function run() {',
    ' context',
    '-old',
    '+new',
    '+more',
    '\\ No newline at end of file',
  ]);
  check('diff parser classifies file headers and hunk rows', diff[0].kind === 'meta' && diff[1].kind === 'meta' && diff[2].kind === 'meta' && diff[3].kind === 'hunk');
  check('diff parser tracks context old/new numbers', diff[4].kind === 'context' && diff[4].oldNo === 10 && diff[4].newNo === 20 && diff[4].prefix === ' ' && diff[4].text === 'context');
  check('diff parser tracks deletion number', diff[5].kind === 'deletion' && diff[5].oldNo === 11 && diff[5].newNo == null && diff[5].prefix === '-');
  check('diff parser tracks consecutive addition numbers', diff[6].kind === 'addition' && diff[6].newNo === 21 && diff[7].newNo === 22 && diff[7].prefix === '+');
  check('no-newline marker is meta', diff[8].kind === 'meta' && diff[8].prefix === '\\');

  const injected = fileExplorerMergeTree('', [], [
    { path: 'added.ts', status: 'added' },
    { path: 'gone.ts', status: 'deleted' },
  ]);
  check('injected ordinary row fails closed on unknown size', injected.find((row) => row.path === 'added.ts')?.sizeUnknown === true);
  check('git-only row keeps its diff action despite unknown source size', injected.find((row) => row.path === 'gone.ts')?.gitOnly === true && injected.find((row) => row.path === 'gone.ts')?.sizeUnknown !== true);

  const largeContext = 'x'.repeat(10 * 1024);
  const firstHunk = ['@@ -1,450 +1,450 @@', ...Array.from({ length: 450 }, (_, i) => ` line-${i + 1}-${largeContext}`)];
  const compactedCache = fileExplorerBoundPageCache({
    [fileExplorerPageCacheKey('sess-compact-diff', 'a.ts', 'diff')]: {
      kind: 'diff', path: 'a.ts', startCursor: 0, lines: firstHunk, totalLines: firstHunk.length,
      truncated: false, loading: false, error: null, transportIncomplete: false,
      generation: 1, sessionId: 'sess-compact-diff', touch: 1,
    },
  }, fileExplorerPageCacheKey('sess-compact-diff', 'a.ts', 'diff'));
  const compacted = fileExplorerReadCachedPage(compactedCache, 'sess-compact-diff', 'a.ts', 'diff');
  const compactedRows = compacted ? parseFileExplorerUnifiedDiff(compacted.lines, compacted.diffStartState) : [];
  check('cache compaction retains only the bounded diff tail', compacted?.lines.length === FILE_EXPLORER_MAX_LINES_PER_ENTRY && compacted.startCursor === 51);
  check('mid-hunk cache checkpoint preserves retained context gutters', compactedRows[0].kind === 'context' && compactedRows[0].oldNo === 51 && compactedRows[0].newNo === 51);
  const multi = parseFileExplorerUnifiedDiff([
    '@@ -0,0 +1,2 @@', '+one', '+two',
    'similarity index 100%', 'rename from old.ts', 'rename to new.ts',
    '@@ -9 +11 @@', '-old', '+new',
    'Binary files a/a.bin and b/a.bin differ',
  ]);
  check('zero-count and omitted-count hunks track gutters', multi[1].newNo === 1 && multi[2].newNo === 2 && multi[7].oldNo === 9 && multi[8].newNo === 11);
  check('rename and binary lines remain bounded meta rows', multi[3].kind === 'meta' && multi[4].kind === 'meta' && multi[5].kind === 'meta' && multi[9].kind === 'meta');
  check('parser preserves one output row per input for highlight alignment', multi.length === 10);
}


// ---- SSR oversized tree, neutral backend limit error, and diff gutters ----
{
  const panelProps = {
    sendCommand: () => Promise.resolve({}),
    hostAuthority: '127.0.0.1:1',
    pageProtocol: 'http:',
    connectionEpoch: 0,
    onOpenPath: () => {},
    onClose: () => {},
  };
  updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(
    prev,
    'sess-large-tree',
    '',
    (state) => fileExplorerApplyDirState(fileExplorerBeginDirState(state, 1), [
      { name: 'large.txt', path: 'large.txt', kind: 'file', sizeBytes: FILE_EXPLORER_MAX_PREVIEW_BYTES + 1 },
    ], false, 1),
  ));
  const largeTreeHtml = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-large-tree',
    slot: emptyFileExplorerSlot(),
  }));
  check('oversized tree file is disabled and accessibly named', largeTreeHtml.includes('data-preview-unavailable="true"') && largeTreeHtml.includes('disabled=""') && largeTreeHtml.includes(FILE_EXPLORER_TOO_LARGE_MESSAGE), largeTreeHtml);
  const panelSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/panels/FileExplorerPanel.tsx'), 'utf8');
  check('unavailable row removes click and context actions and disables status', panelSource.includes('onClick={openRow}') && panelSource.includes('onContextMenu={disabled ? undefined') && panelSource.includes('disabled={disabled}') && panelSource.includes('disabledReason={disabledReason}'));
  check('unknown-size injected rows use the distinct unavailable message', panelSource.includes('row.sizeUnknown') && panelSource.includes('FILE_EXPLORER_SIZE_UNAVAILABLE_MESSAGE') && FILE_EXPLORER_SIZE_UNAVAILABLE_MESSAGE !== FILE_EXPLORER_TOO_LARGE_MESSAGE);

  const disabledStatusHtml = renderToStaticMarkup(createElement(ChangeStatusButton, {
    change: { path: 'large.txt', status: 'modified' },
    onOpenDiff: () => {},
    disabledReason: FILE_EXPLORER_TOO_LARGE_MESSAGE,
  }));
  check('oversized changed-file status is disabled with too-large accessible name', disabledStatusHtml.includes('disabled=""') && disabledStatusHtml.includes(`aria-label="${FILE_EXPLORER_TOO_LARGE_MESSAGE}"`) && disabledStatusHtml.includes(`title="${FILE_EXPLORER_TOO_LARGE_MESSAGE}"`), disabledStatusHtml);

  const seedPage = (sessionId: string, path: string, view: 'file' | 'diff', lines: string[], error?: FileExplorerPageError) => {
    fileExplorerSyncPageCacheWithSlots({ [sessionId]: { tabs: [{ path, view }], activePath: path, mode: 'open' } });
    const request = { sessionId, path, view, generation: 1 };
    let cache = fileExplorerBeginCachedPage(emptyFileExplorerPageCache(), request);
    cache = error
      ? fileExplorerFailCachedPage(cache, request, error)
      : fileExplorerApplyCachedPage(cache, request, { path, lines, truncated: false });
    writeModuleFileExplorerPageCache(cache);
  };
  seedPage('sess-limit-error', 'huge.txt', 'file', [], { message: 'line exceeds response limit', code: 'file_line_too_large' });
  const errorHtml = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-limit-error',
    slot: { tabs: [{ path: 'huge.txt', view: 'file' }], activePath: 'huge.txt', mode: 'open' },
  }));
  check('typed too-large code becomes neutral too-large message', errorHtml.includes(`file-explorer__muted">${FILE_EXPLORER_TOO_LARGE_MESSAGE}`) && !errorHtml.includes('line exceeds response limit'), errorHtml);
  check('typed too-large code has no Retry', !errorHtml.includes('>Retry</button>'), errorHtml);

  seedPage('sess-limit-prose', 'huge.txt', 'file', [], { message: 'completely different backend prose', code: 'file_line_too_large' });
  const proseHtml = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-limit-prose',
    slot: { tabs: [{ path: 'huge.txt', view: 'file' }], activePath: 'huge.txt', mode: 'open' },
  }));
  check('typed too-large code ignores backend prose', proseHtml.includes(`file-explorer__muted">${FILE_EXPLORER_TOO_LARGE_MESSAGE}`) && !proseHtml.includes('completely different backend prose') && !proseHtml.includes('>Retry</button>'), proseHtml);

  seedPage('sess-old-phrase', 'huge.txt', 'file', [], { message: 'line exceeds response limit' });
  const oldPhraseHtml = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-old-phrase',
    slot: { tabs: [{ path: 'huge.txt', view: 'file' }], activePath: 'huge.txt', mode: 'open' },
  }));
  check('old too-large phrase without code is generic Retry', oldPhraseHtml.includes('file-explorer__error') && oldPhraseHtml.includes('line exceeds response limit') && oldPhraseHtml.includes('>Retry</button>') && !oldPhraseHtml.includes(`file-explorer__muted">${FILE_EXPLORER_TOO_LARGE_MESSAGE}`), oldPhraseHtml);

  seedPage('sess-unknown-code', 'gone.txt', 'file', [], { message: 'file is unavailable' });
  const unknownHtml = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-unknown-code',
    slot: { tabs: [{ path: 'gone.txt', view: 'file' }], activePath: 'gone.txt', mode: 'open' },
  }));
  check('message without code stays generic Retry', unknownHtml.includes('file-explorer__error') && unknownHtml.includes('file is unavailable') && unknownHtml.includes('>Retry</button>'), unknownHtml);
  seedPage('sess-diff', 'src/a.ts', 'diff', ['@@ -1,2 +1,2 @@', ' same', '-old', '+new', '\\ No newline at end of file']);
  const diffHtml = renderToStaticMarkup(createElement(FileExplorerPanel, {
    ...panelProps,
    sessionId: 'sess-diff',
    slot: { tabs: [{ path: 'src/a.ts', view: 'diff' }], activePath: 'src/a.ts', mode: 'open' },
  }));
  check('diff SSR renders old/new gutters and prefix', diffHtml.includes('file-explorer__diff') && diffHtml.includes('file-explorer__line-no--old') && diffHtml.includes('file-explorer__line-no--new') && diffHtml.includes('file-explorer__line-prefix'), diffHtml);
  check('diff SSR renders hunk cards with addition deletion and meta', diffHtml.includes('file-explorer__hunk') && diffHtml.includes('file-explorer__hunk-header') && diffHtml.includes('@@ -1,2 +1,2 @@') && diffHtml.includes('file-explorer__line--addition') && diffHtml.includes('file-explorer__line--deletion') && diffHtml.includes('file-explorer__line--meta'), diffHtml);
  const grouped = groupFileExplorerDiffRows(parseFileExplorerUnifiedDiff(['diff --git a/a.ts b/a.ts', '@@ -1 +1 @@', '-old', '+new']));
  check('diff grouping puts file headers in prelude then a hunk card', grouped.length === 2 && grouped[0].header === null && grouped[0].rows[0].kind === 'meta' && grouped[1].header?.kind === 'hunk' && grouped[1].rows.length === 2);
  fileExplorerSyncPageCacheWithSlots({});
}


// ---- viewState slot persistence across close/unmount ----
{
  const empty = emptyFileExplorerViewState();
  check('empty view expands root only', empty.expandedDirs[''] === true && Object.keys(empty.expandedDirs).length === 1);
  check('empty view uses the default tree percent', empty.treePct === FILE_EXPLORER_TREE_PCT_DEFAULT);
  check('empty view has zero tree scroll', empty.treeScrollTop === 0);
  check('empty view has an empty surface map', Object.keys(empty.surfaceByPath).length === 0);
  check('empty slot has no viewState field', emptyFileExplorerSlot().viewState === undefined);
  check('viewForOwner falls back to empty defaults', fileExplorerViewForOwner({}, 'sess-x').treePct === FILE_EXPLORER_TREE_PCT_DEFAULT);

  // Store cutover: view geometry is the module store keyed by the owner,
  // NOT the session slot — a toggle never touches App/Chat slot state.
  writeModuleFileExplorerViewCache({});
  let listeners = 0;
  const unsubscribe = subscribeFileExplorerViewCache(() => { listeners += 1; });
  patchFileExplorerViewCache('sess-a', { expandedDirs: { '': true, src: true }, treePct: 40, treeScrollTop: 88 });
  check('patch writes synchronously and publishes once', listeners === 1);
  let view = fileExplorerViewForOwner(readModuleFileExplorerViewCache(), 'sess-a');
  check('patch writes geometry under the owner key', view.treePct === 40 && view.treeScrollTop === 88 && view.expandedDirs.src === true);
  check('slot applyDrawerMode never touches the view store', applyDrawerMode(emptyFileExplorerSlot(), 'closed').viewState === undefined && readModuleFileExplorerViewCache()['sess-a'] != null);

  // Close/reopen: the store keeps exact geometry across a full unmount.
  view = fileExplorerViewForOwner(readModuleFileExplorerViewCache(), 'sess-a');
  check('close keeps store geometry', view.treeScrollTop === 88 && view.expandedDirs.src === true);

  // Owner isolation: A and B (and Chat mapping keys) never share geometry.
  patchFileExplorerViewCache('sess-b', { treePct: 30 });
  view = fileExplorerViewForOwner(readModuleFileExplorerViewCache(), 'sess-a');
  const viewB = fileExplorerViewForOwner(readModuleFileExplorerViewCache(), 'mapping-1');
  check('B patch does not leak into A', viewB.treePct === FILE_EXPLORER_TREE_PCT_DEFAULT && view.treePct === 40 && fileExplorerViewForOwner(readModuleFileExplorerViewCache(), 'sess-b').treePct === 30);

  // Toggle purity: expand reports expanded=true and adds the key; collapse
  // removes it and reports expanded=false.
  let expanded = fileExplorerToggleExpandedDir(view, 'src/lib');
  check('toggle expands a cold dir', expanded.expanded === true && expanded.view.expandedDirs['src/lib'] === true && expanded.view.expandedDirs.src === true);
  const collapsed = fileExplorerToggleExpandedDir(expanded.view, 'src/lib');
  check('toggle collapses an expanded dir', collapsed.expanded === false && collapsed.view.expandedDirs['src/lib'] === undefined && collapsed.view.expandedDirs.src === true);

  // Functional update: two rapid toggles never lose an update.
  updateFileExplorerViewCache('sess-a', (v) => fileExplorerToggleExpandedDir(v, 'x').view);
  updateFileExplorerViewCache('sess-a', (v) => fileExplorerToggleExpandedDir(v, 'y').view);
  view = fileExplorerViewForOwner(readModuleFileExplorerViewCache(), 'sess-a');
  check('rapid updates both land', view.expandedDirs.x === true && view.expandedDirs.y === true);
  updateFileExplorerViewCache('sess-a', (v) => fileExplorerToggleExpandedDir(v, 'x').view);
  view = fileExplorerViewForOwner(readModuleFileExplorerViewCache(), 'sess-a');
  check('toggle through the store removes the key', view.expandedDirs.x === undefined && view.expandedDirs.y === true);
  unsubscribe();

  // Eviction: the fifth session's slot drop removes its view entry; the
  // retained sessions keep theirs.
  fileExplorerSyncViewCacheWithSlots({ 'sess-a': emptyFileExplorerSlot(), 'sess-b': emptyFileExplorerSlot() });
  const afterEvict = readModuleFileExplorerViewCache();
  check('evicted owner drops view geometry', afterEvict['sess-a'] != null && afterEvict['sess-b'] != null && afterEvict['mapping-1'] === undefined);

  // Patch clamps: tree percent bounded, scroll floored (same contract the
  // slot path applied).
  const clamped = fileExplorerApplyViewPatch(fileExplorerViewForOwner(readModuleFileExplorerViewCache(), 'sess-a'), { treePct: 99, treeScrollTop: -12.7 });
  check('view clamps tree percent and floors scroll', clamped.treePct === FILE_EXPLORER_TREE_PCT_MAX && clamped.treeScrollTop === 0);
  writeModuleFileExplorerViewCache({});
}

// ---- SSR: directory expansion renders from the view store alone ----
// Two renders with an IDENTICAL slot prop differ only because the module
// view store changed — the toggle commit never touches App/Chat top-level
// state, so a large transcript parent render count stays flat (the browser
// lane measures the live parent counter; this proves the store→render edge).
{
  const seedDir = (sessionId: string, path: string, entries: Array<{ name: string; kind: 'file' | 'dir' }>) => {
    updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(
      prev,
      sessionId,
      path,
      (state) => fileExplorerApplyDirState(
        fileExplorerBeginDirState(state, 1),
        entries.map((entry) => ({
          name: entry.name,
          path: path ? `${path}/${entry.name}` : entry.name,
          kind: entry.kind,
        })),
        false,
        1,
      ),
    ));
  };
  const panelProps = {
    sendCommand: () => Promise.resolve({}),
    hostAuthority: '127.0.0.1:1',
    pageProtocol: 'http:',
    connectionEpoch: 0,
    onOpenPath: () => {},
    onClose: () => {},
  };
  writeModuleFileExplorerViewCache({});
  const slot: FileExplorerSessionSlot = Object.freeze({
    tabs: [] as FileExplorerTab[],
    activePath: null,
    mode: 'open' as const,
  });
  seedDir('sess-ssr', '', [{ name: 'README.md', kind: 'file' }, { name: 'src', kind: 'dir' }]);
  seedDir('sess-ssr', 'src', [{ name: 'a.ts', kind: 'file' }]);
  patchFileExplorerViewCache('sess-ssr', { expandedDirs: { '': true, src: true }, treePct: 40 });
  const openHtml = renderToStaticMarkup(createElement(FileExplorerPanel, { ...panelProps, sessionId: 'sess-ssr', slot }));
  check('expanded dir renders aria-expanded true from the store', openHtml.includes('aria-expanded="true"') && openHtml.includes('src/a.ts') && openHtml.includes('--file-explorer-tree-pct:40%'));
  const treeOpen = openHtml.indexOf('data-testid="file-explorer-tree"');
  const searchIdx = openHtml.indexOf('data-testid="file-explorer-search"');
  const treeIdx = openHtml.indexOf('class="file-explorer__tree-list"');
  check(
    'search is the tree column toolbar ahead of the scrolling list',
    searchIdx >= 0
      && openHtml.includes('aria-label="Search workspace files"')
      && !openHtml.includes('file-explorer__tree-edge')
      && treeOpen >= 0
      && treeOpen < searchIdx
      && treeIdx > searchIdx,
  );
  // The exact toggle write path: synchronous store update, then re-render.
  updateFileExplorerViewCache('sess-ssr', (view) => fileExplorerToggleExpandedDir(view, 'src').view);
  const closedHtml = renderToStaticMarkup(createElement(FileExplorerPanel, { ...panelProps, sessionId: 'sess-ssr', slot }));
  check('collapsed dir renders aria-expanded false with no child rows', closedHtml.includes('aria-expanded="false"') && !closedHtml.includes('src/a.ts') && !closedHtml.includes('aria-expanded="true"'));
  check('slot prop is byte-identical across the toggle renders', JSON.stringify(slot) === '{"tabs":[],"activePath":null,"mode":"open"}');
  // Re-expand restores the cached subtree render with zero new listing state.
  updateFileExplorerViewCache('sess-ssr', (view) => fileExplorerToggleExpandedDir(view, 'src').view);
  const reopenedHtml = renderToStaticMarkup(createElement(FileExplorerPanel, { ...panelProps, sessionId: 'sess-ssr', slot }));
  check('re-expand restores child rows from the cached dir state', reopenedHtml.includes('aria-expanded="true"') && reopenedHtml.includes('src/a.ts'));

  const panelSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/panels/FileExplorerPanel.tsx'), 'utf8');
  check('panel toggle writes the view store, never parent slot state', panelSource.includes('updateFileExplorerViewCache(viewOwnerKey') && panelSource.includes('patchFileExplorerViewCache(viewOwnerKey, patch)') && !panelSource.includes('onViewStateChange'));
  writeModuleFileExplorerViewCache({});
}
void Promise.all(pending).then(() => {
  console.log(`\nfileExplorer.test: ${ran} assertions, ${failures.length} failure(s)`);
  if (failures.length) {
    console.error(failures.join('\n'));
    process.exit(1);
  }
  process.exit(0);
});
