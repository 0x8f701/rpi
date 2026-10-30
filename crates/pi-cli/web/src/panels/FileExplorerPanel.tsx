// Right-side file-explorer drawer.
//
// Wire: file_explorer_list | read | changes | diff | stage_download
// (crates/pi-cli/web/src/fileExplorer.ts — frozen contract).
//
// App owns per-session tabs / active tab / drawer mode so close and
// A→B session switch never drop open files. This component fetches
// listing/diff/read for the ACTIVE session and stamps sessionId through
// sendCommand (same as every other panel). Open/close authority is the
// App sidebar Files toggle, the header ×, and Escape (which restores
// focus to the Files toggle).
//
// Layout: static Files header (title, icon-only Refresh, ⛶, × last).
// Search and the tree collapse control sit in the tree column, under
// that header and above the listing. Source | Preview | Ask | Map is a
// content-local toolbar above the open file. Phone ≤720 shows the tree
// OR the selected file, never a stacked half-and-half split; desktop
// ≥721 keeps the side-by-side tree/content split. A collapsed desktop
// tree stays a slim rail so the toggle remains reachable.
// Changed status is a fixed trailing column on tree rows (one Git-aware
// merged tree; no orphan list).
//
// File contents: previewable .md/.markdown/.html/.htm/.svg default to Preview
// (MarkdownBody, sandboxed HTML iframe, SVG img). Source is the existing
// line-numbered view. Source | Preview | Ask | Map lives above the file content.
// Unknown files stay React text after safeText(). Paging is infinite-scroll;
// there is no Load more button.

import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  type KeyboardEvent as ReactKeyboardEvent,
  type PointerEvent as ReactPointerEvent,
} from 'react';
import { MarkdownBody } from '../MarkdownBody';
import { diffPathLanguage, highlightDiffLineFragments, renderMarkdown } from '../markdown';
import { safeText } from '../redact';
import { isTransportError } from '../transportError';
import { rpcErrorCode } from '../types';
import { diffTreePaddingLeft } from '../codeReview';
import {
  fileConversationAbortCommand,
  fileConversationCloseAlreadyRetired,
  fileConversationPromptAccepted,
  fileConversationPromptCommand,
  fileConversationPromptTooLarge,
  fileConversationSnapshotCommand,
  fileConversationOpenCommand,
  FILE_CONVERSATION_PROMPT_MAX_BYTES,
  fileExplorerConversationDraft,
  fileExplorerWithConversationDraft,
  newFileConversationId,
  normalizeFileConversationSnapshot,
  FILE_EXPLORER_DEFAULT_MAX_LINES,
  fileExplorerContentSurface,
  fileExplorerPreviewKind,
  fileExplorerStatusLetter,
  fileExplorerWithPathSurface,
  FILE_EXPLORER_TREE_PCT_DEFAULT,
  FILE_EXPLORER_TREE_PCT_MAX,
  FILE_EXPLORER_TREE_PCT_MIN,
  clampFileExplorerTreePct,
  emptyFileExplorerViewState,
  fileExplorerApplyDirState,
  fileExplorerAcquireDirInflight,
  fileExplorerBeginDirState,
  fileExplorerBuildChangeIndex,
  fileExplorerDirInflightKey,
  fileExplorerEmptyDirState,
  fileExplorerFailDirState,
  fileExplorerToggleExpandedDir,
  type FileExplorerChangeIndex,
  groupFileExplorerDiffRows,
  parseFileExplorerUnifiedDiff,
  FILE_EXPLORER_TOO_LARGE_MESSAGE,
  FILE_EXPLORER_SIZE_UNAVAILABLE_MESSAGE,
  type FileExplorerTab,
  fileExplorerAcquireInflight,
  fileExplorerBeginCachedPage,
  fileExplorerCanLoadMore,
  fileExplorerBlameCommand,
  normalizeBlame,
  type FileExplorerBlameResult,
  fileExplorerChangesCommand,
  fileExplorerDiffCommand,
  normalizeFileExplorerMap,
  fileExplorerSamePath,
  fileExplorerMapCommand,
  fileExplorerMapResponseCurrent,
  type FileExplorerMapRequest,
  type FileExplorerMapResult,
  fileExplorerDisplayPath,
  fileExplorerCommandForSession,
  fileExplorerDirsForSession,
  fileExplorerCursorForLine,
  fileExplorerInflightKey,
  fileExplorerListCommand,
  fileExplorerMergeTree,
  fileExplorerLoadedDescendantMatches,
  fileExplorerNormalizeTreeQuery,
  fileExplorerTreeQueryMatches,
  fileExplorerTreeTone,
  fileExplorerReadCachedPage,
  fileExplorerReadCommand,
  fileExplorerReleaseInflight,
  fileExplorerRetryableTransportDirPaths,
  fileExplorerWriteDirState,
  fileExplorerSetCachedScrollTop,
  fileExplorerSettleInflightCache,
  fileExplorerShouldFetchPage,
  fileExplorerShouldLoadNext,
  fileExplorerShouldRetryTransportDirs,
  fileExplorerShouldRetryTransportPage,
  fileExplorerShouldRestoreFileScroll,
  fileExplorerStageDownloadCommand,
  fileExplorerTransportRetryCursor,
  fileExplorerTruncationInconsistent,
  getFileExplorerPageCacheSnapshot,
  getFileExplorerDirCacheSnapshot,
  getFileExplorerViewCacheSnapshot,
  nextFileExplorerRequestGeneration,
  normalizeChanges,
  normalizeList,
  normalizeStageDownload,
  patchFileExplorerViewCache,
  subscribeFileExplorerPageCache,
  subscribeFileExplorerDirCache,
  subscribeFileExplorerViewCache,
  stepFileExplorerTreePct,
  triggerFileExplorerDownload,
  updateFileExplorerPageCache,
  updateFileExplorerDirCache,
  updateFileExplorerViewCache,
  type FileExplorerChange,
  type FileExplorerViewState,
  type FileExplorerChangeStatus,
  type FileExplorerDirState,
  type FileExplorerDiffRow,
  type FileExplorerSessionSlot,
  type FileExplorerContentSurface,
  type FileExplorerTreeRow,
  type FileConversationLineRange,
  type FileExplorerPageState,
} from '../fileExplorer';

import { FileTypeIcon } from '../components/FileTypeIcon';
import { FileConversationDock, type FileConversationSnapshot } from './FileConversationDock';
import { FileSelectionActions } from './FileSelectionActions';
import { FileDependencyMap } from './FileDependencyMap';

// Backend caps the changed-file catalog (MAX_CHANGED_FILES = 5000) and marks
// the response truncated; the panel mirrors the cap defensively so a
// non-conforming response can never render unbounded change rows.
const CHANGES_CAP = 5000;

/** Desktop tree/content split (percent of the body). One authority in
 *  fileExplorer.ts: the session slot persists the inner tree width through
 *  close/unmount, so the panel re-exports the shared constants. */
export {
  FILE_EXPLORER_TREE_PCT_DEFAULT,
  FILE_EXPLORER_TREE_PCT_MAX,
  FILE_EXPLORER_TREE_PCT_MIN,
  FILE_EXPLORER_TREE_PCT_STEP,
} from '../fileExplorer';
const FILE_EXPLORER_SPLIT_QUERY = '(max-width: 720px)';

/** Pointer math: tree percent from a clientX inside the split body. */
export function fileExplorerTreePctFromPointer(
  clientX: number,
  bodyLeft: number,
  bodyWidth: number,
): number {
  if (!(bodyWidth > 0) || !Number.isFinite(clientX) || !Number.isFinite(bodyLeft)) {
    return FILE_EXPLORER_TREE_PCT_DEFAULT;
  }
  return clampFileExplorerTreePct(((clientX - bodyLeft) / bodyWidth) * 100);
}

/** Keyboard mapping for the vertical tree/content separator. Unhandled keys → null. */
export function fileExplorerTreePctForKey(current: number, key: string): number | null {
  if (key === 'ArrowLeft') return stepFileExplorerTreePct(current, -1);
  if (key === 'ArrowRight') return stepFileExplorerTreePct(current, 1);
  if (key === 'Home') return FILE_EXPLORER_TREE_PCT_MIN;
  if (key === 'End') return FILE_EXPLORER_TREE_PCT_MAX;
  return null;
}

export type FileExplorerBodyLayout = 'split' | 'tree' | 'content';

export function fileExplorerBodyLayout(
  isNarrow: boolean,
  treeCollapsed: boolean,
): FileExplorerBodyLayout {
  if (!isNarrow) return 'split';
  return treeCollapsed ? 'content' : 'tree';
}

/** App sidebar Files reopen toggle — the DEFAULT toggle id. Chat passes its
 *  own distinct id (CHAT_FILE_EXPLORER_TOGGLE_ID) so Escape focus restores
 *  that route's own toggle. */
export const FILE_EXPLORER_TOGGLE_ID = 'file-explorer-toggle-btn';

export const FILE_EXPLORER_TREE_TOGGLE_ID = 'file-explorer-tree-toggle';
export const FILE_EXPLORER_CLOSE_BTN_ID = 'file-explorer-drawer-close-btn';
export const FILE_EXPLORER_REFRESH_BTN_ID = 'file-explorer-refresh-btn';
/** One suffix-keyed derivation for every per-route drawer control id
 *  (refresh, tree toggle, drawer close, expand, line anchors). App keeps the
 *  DEFAULT toggle id, so every derived id stays the historical constant above
 *  (`file-explorer-refresh-btn`, …). Chat passes
 *  CHAT_FILE_EXPLORER_TOGGLE_ID, so its drawer derives
 *  `chat-file-explorer-toggle-btn-*` ids. App and Chat render in separate
 *  documents, so the prefixes are redundant-but-harmless: the derivation
 *  keeps every route's ids unique against any document-wide lookup (SSR
 *  snapshots, extensions, stale helpers). */
export function fileExplorerControlId(toggleId: string, suffix: string): string {
  return toggleId === FILE_EXPLORER_TOGGLE_ID ? `file-explorer-${suffix}` : `${toggleId}-${suffix}`;
}

/** Active-line anchor id for this route's code/diff viewport. App's default
 *  derivation stays the historical `file-explorer-line-N`; Chat derives
 *  `chat-file-explorer-toggle-btn-line-N` from its own toggle id. */
export function fileExplorerLineId(toggleId: string, lineNo: number): string {
  return fileExplorerControlId(toggleId, `line-${lineNo}`);
}

/** Document-wide fallback for `toggleId`. Bare sections (SSR/tests, documents
 *  without the route shell) have no route view to scope the lookup, so the
 *  query falls back to the document, keyed by the same toggle id. */
export function fileExplorerToggleButton(toggleId: string = FILE_EXPLORER_TOGGLE_ID): HTMLElement | null {
  if (typeof document === 'undefined') return null;
  return document.getElementById(toggleId);
}

/** Document Escape triage. Field/content-editable targets never consume it.
 *  When `onSelectionDismiss` reports a dismissed selection/blame the
 *  keystroke is consumed there and the drawer stays open; otherwise the
 *  drawer closes and focus returns to the Files toggle. */
export function fileExplorerHandleDocumentKeyDown(
  event: Pick<KeyboardEvent, 'key' | 'preventDefault' | 'target'>,
  onClose: () => void,
  toggle: { focus: () => void } | null,
  onSelectionDismiss?: () => boolean,
): boolean {
  if (event.key !== 'Escape') return false;
  const target = event.target as { tagName?: string; isContentEditable?: boolean } | null;
  const tag = typeof target?.tagName === 'string' ? target.tagName.toUpperCase() : '';
  if (target?.isContentEditable === true || tag === 'TEXTAREA' || tag === 'INPUT' || tag === 'SELECT') {
    return false;
  }
  // A live selection/blame owns Escape first: the dismiss reports whether it
  // cleared anything, and only then is the keystroke consumed (drawer open).
  if (onSelectionDismiss?.() === true) {
    event.preventDefault();
    return true;
  }
  event.preventDefault();
  onClose();
  toggle?.focus();
  return true;
}

/** Escape focus scope for the panel's document keydown: the panel root
 *  itself. After the URL-only cutover one route renders per document, so the
 *  document-wide toggle lookup that backs this scope is unambiguous — each
 *  host renders its own Files toggle keyed by its `toggleId` prop. Returns
 *  null for a null root (SSR/tests); the caller then falls back to the
 *  document lookup. */
export function fileExplorerKeydownScope(root: HTMLElement | null): HTMLElement | null {
  return root;
}

export const FILE_EXPLORER_SELECTION_EDGE_PX = 8;

export interface FileExplorerSelectionRect {
  top: number;
  left: number;
  width: number;
  height: number;
}

export interface FileExplorerSelectionAnchorInput {
  /** Positioning context (the content viewport), viewport-origin coords. */
  viewport: FileExplorerSelectionRect;
  /** Union of the painted source-line rects for the selection, viewport
   *  origin; null when no selected line is painted in the visible source. */
  selection: FileExplorerSelectionRect | null;
  /** Measured bar size. */
  bar: { width: number; height: number };
  /** Measured blame-popup size; anchored to the bar with an edge gap in
   *  the direction that has room, clamped into the viewport. */
  popup?: { width: number; height: number };
  /** Viewport inset clamp; defaults to FILE_EXPLORER_SELECTION_EDGE_PX. */
  edge?: number;
}

export interface FileExplorerSelectionAnchor {
  visible: boolean;
  top: number;
  left: number;
  placement: 'below' | 'above';
  /** Blame-popup box, viewport-origin, clamped into the viewport inset.
   *  maxWidth/maxHeight are the clamped caps — the popup scrolls its own
   *  overflow inside them. Present only when a popup size was supplied and
   *  a direction has at least `edge` of room. */
  popup?: { top: number; left: number; maxWidth: number; maxHeight: number };
}

/** Pure geometry for the floating selection bar. Prefers below the
 *  selection when the viewport has room for bar + edge gap, otherwise flips
 *  above; both axes clamp into the viewport inset by `edge`. An optional
 *  popup size anchors the blame overlay to the bar with an edge gap in the
 *  direction that has room (flipping independently), clamped into the same
 *  inset. Not visible when the selection is null or does not intersect the
 *  viewport. */
export function fileExplorerSelectionAnchor(
  input: FileExplorerSelectionAnchorInput,
): FileExplorerSelectionAnchor {
  const edge = input.edge ?? FILE_EXPLORER_SELECTION_EDGE_PX;
  const selection = input.selection;
  if (selection == null || input.bar.height <= 0 || selection.width <= 0 || selection.height <= 0) {
    return { visible: false, top: 0, left: 0, placement: 'below' };
  }
  const viewportBottom = input.viewport.top + input.viewport.height;
  const viewportRight = input.viewport.left + input.viewport.width;
  const selectionBottom = selection.top + selection.height;
  const selectionRight = selection.left + selection.width;
  if (
    input.viewport.height <= 0
    || selectionBottom <= input.viewport.top
    || selection.top >= viewportBottom
    || selectionRight <= input.viewport.left
    || selection.left >= viewportRight
  ) {
    return { visible: false, top: 0, left: 0, placement: 'below' };
  }
  const placement: FileExplorerSelectionAnchor['placement'] =
    viewportBottom - selectionBottom >= input.bar.height + edge ? 'below' : 'above';
  const topMin = input.viewport.top + edge;
  const topMax = viewportBottom - edge - input.bar.height;
  const rawTop = placement === 'below' ? selectionBottom + edge : selection.top - edge - input.bar.height;
  const top = topMax < topMin ? topMin : Math.min(Math.max(rawTop, topMin), topMax);
  const leftMin = input.viewport.left + edge;
  const leftMax = viewportRight - edge - input.bar.width;
  const left = leftMax < leftMin ? leftMin : Math.min(Math.max(selection.left, leftMin), leftMax);
  const popupSize = input.popup;
  if (popupSize == null || popupSize.width <= 0 || popupSize.height <= 0) {
    return { visible: true, top, left, placement };
  }
  const barBottom = top + input.bar.height;
  const spaceBelow = viewportBottom - edge - barBottom;
  const spaceAbove = top - input.viewport.top - edge;
  const popupDirection: 'below' | 'above' =
    popupSize.height <= spaceBelow ? 'below'
    : popupSize.height <= spaceAbove ? 'above'
    : spaceBelow >= spaceAbove ? 'below' : 'above';
  const maxHeight = popupDirection === 'below' ? spaceBelow : spaceAbove;
  if (maxHeight < edge) {
    return { visible: true, top, left, placement };
  }
  const maxWidth = Math.max(input.viewport.width - 2 * edge, 0);
  const popupLeftMin = input.viewport.left + edge;
  const popupLeftMax = viewportRight - edge - Math.min(popupSize.width, maxWidth);
  const popupLeft = popupLeftMax < popupLeftMin
    ? popupLeftMin
    : Math.min(Math.max(left, popupLeftMin), popupLeftMax);
  const popupTop = popupDirection === 'below'
    ? barBottom + edge
    : Math.max(input.viewport.top + edge, top - edge - Math.min(popupSize.height, maxHeight));
  return {
    visible: true,
    top,
    left,
    placement,
    popup: { top: popupTop, left: popupLeft, maxWidth, maxHeight },
  };
}

export interface FileExplorerPanelProps {
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  sessionId: string | null;
  slot: FileExplorerSessionSlot;
  hostAuthority: string;
  pageProtocol: string;
  /** Exact verified origin for staged downloads (`https://host:port`). */
  downloadOrigin?: string;
  workspaceCwd?: string;
  /** Drawer view geometry is NOT a prop: it lives in the module
   *  view-geometry external store (fileExplorer.ts) keyed by this host's
   *  owner key (App sessionId / Chat mapping id passed as sessionId), so a
   *  directory toggle never rerenders the App/Chat slot holders and a real
   *  conditional close/unmount still restores exact geometry on reopen. */
  onOpenPath: (path: string, line?: number, view?: FileExplorerTab['view']) => void;
  onClose: () => void;
  /** Host slot patch bound to THIS panel's owner key (App: current sessionId;
   *  Chat: the mapping id passed as sessionId). The panel uses it ONLY to
   *  allocate a conversation id on the active tab (lazy, on the first Ask
   *  send) and to roll an unused allocation back; every other slot mutation
   *  stays host-owned, and removed tabs retire their conversations through
   *  the host's own patch path. */
  onPatchSlot: (updater: (slot: FileExplorerSessionSlot) => FileExplorerSessionSlot) => void;
  /** Central host error surface (App toast / Chat form error). Required:
   *  file-conversation errors are never rendered inline by this panel. */
  onConversationError: (message: string) => void;
  /** Real RPC session id for conversation entry data attributes. App's panel
   *  sessionId IS the real session; Chat's is a mapping id, so Chat passes
   *  the runner session here. Defaults to sessionId. */
  conversationSessionId?: string;
  /** Captured Chat markdown owner. App omits both; Chat passes the selected
   *  mapping's runner and mapping ids so preview links resolve that owner
   *  instead of an outer session ancestor. */
  markdownRunnerId?: string;
  markdownMappingId?: string;
  /**
   * Authoritative reconnect generation from App. A new epoch retries a
   * transport-stuck directory listing and the active tab read exactly once.
   * Same epoch never auto-retries.
   */
  connectionEpoch: number;
  /** Unused: overlay layout is App-owned (`app-layout--files-overlay`).
   *  Kept so ChatApp's existing `overlay={false}` still type-checks. */
  overlay?: boolean;
  /** Chat Files column: exclusive tree OR file, ignoring viewport width. */
  exclusivePanes?: boolean;
  /** DOM id of the Files reopen toggle rendered by THIS host. Escape focus
   *  restore looks the toggle up by this id (App keeps the
   *  FILE_EXPLORER_TOGGLE_ID default; Chat passes CHAT_FILE_EXPLORER_TOGGLE_ID,
   *  so each host restores its own toggle). */
  toggleId?: string;
  /** Full-app expand (user requirement): when true the App-level layout
   *  class hides the sidebar + transcript + composer and the file tree +
   *  content viewports fill the entire app area. The SAME-ROW button
   *  becomes the exit/restore control. Reset is App-owned (panel close). */
  expanded?: boolean;
  onToggleExpanded?: () => void;
}

type DirNode = FileExplorerDirState;

function errorMessage(error: unknown): string {
  if (error && typeof error === 'object' && 'message' in error && typeof error.message === 'string' && error.message.trim() !== '') {
    return error.message;
  }
  return 'request failed';
}

/** Settlement decision for one staged-download rejection. A stale-session
 *  settlement (the response belongs to a session that is no longer
 *  rendered) is exact non-fatal suppression → null. Any rejection for the
 *  rendered session is user-owned — transport included — and returns the
 *  actionable error-row message (the tree's Download control is the retry
 *  affordance). */
export function fileExplorerDownloadSettledError(
  renderedSession: boolean,
  error: unknown,
): string | null {
  if (!renderedSession) return null;
  return errorMessage(error);
}

export type FileExplorerContentPaneKind = 'hint' | 'loading' | 'error' | 'empty' | 'ready';

/** Mutually exclusive content-pane status. A selected file never yields a blank viewport.
 *  Transport-incomplete with no lines is not success-empty: it is an
 *  explicit Retry surface, not a spinner. Prior lines stay ready.
 *  'hint' (no selection) intentionally renders NOTHING: the hint copy was
 *  removed without replacement — the workspace tree is the affordance. */
export function fileExplorerContentPaneKind(
  activePath: string | null,
  fileState: Pick<FileExplorerPageState, 'loading' | 'error' | 'lines' | 'transportIncomplete'> | null,
): FileExplorerContentPaneKind {
  if (!activePath) return 'hint';
  if (fileState == null || (fileState.loading && fileState.lines.length === 0)) return 'loading';
  if (fileState.error) return 'error';
  if (fileState.transportIncomplete && fileState.lines.length === 0) return 'error';
  if (fileState.lines.length === 0) return 'empty';
  return 'ready';
}

export const FILE_EXPLORER_LOADING_DELAY_MS = 151;

export function fileExplorerTreeLoadingIdentity(
  sessionId: string | null,
  root: Pick<FileExplorerDirState, 'loading' | 'generation' | 'entries'> | null | undefined,
  changesLoading: boolean,
  changesGeneration: number,
  changesCount: number,
): string | null {
  if (sessionId == null) return null;
  if ((root?.entries?.length ?? 0) > 0 || changesCount > 0) return null;
  if (root?.loading) {
    return `${fileExplorerDirInflightKey(sessionId, '')}\0${root.generation}`;
  }
  if (changesLoading) {
    return `${sessionId}\0changes\0${changesGeneration}`;
  }
  return null;
}

export function fileExplorerIsRefreshBusy(
  treePainted: boolean,
  contentPainted: boolean,
  treeLoading: boolean,
  contentLoading: boolean,
): boolean {
  return (treeLoading && treePainted) || (contentLoading && contentPainted);
}

export function fileExplorerContentLoadingIdentity(
  sessionId: string | null,
  path: string | null,
  view: FileExplorerTab['view'] | undefined,
  paneKind: FileExplorerContentPaneKind,
  generation: number | undefined,
): string | null {
  if (sessionId == null || path == null || paneKind !== 'loading') return null;
  return `${fileExplorerInflightKey(sessionId, path, view ?? 'file')}\0${generation ?? 0}`;
}

export function useFileExplorerDelayedLoading(identity: string | null): boolean {
  const [shownIdentity, setShownIdentity] = useState<string | null>(null);
  useEffect(() => {
    if (identity == null) {
      setShownIdentity(null);
      return undefined;
    }
    setShownIdentity((current) => (current === identity ? current : null));
    const timer = window.setTimeout(() => {
      setShownIdentity(identity);
    }, FILE_EXPLORER_LOADING_DELAY_MS);
    return () => window.clearTimeout(timer);
  }, [identity]);
  return identity != null && shownIdentity === identity;
}

export function FileExplorerPaneLoading({ pane }: { pane: 'tree' | 'content' }) {
  return (
    <div
      className="file-explorer__pane-loading"
      data-testid={`file-explorer-${pane}-loading`}
      data-pane={pane}
      aria-hidden="true"
    >
      <span className="file-explorer__pane-shimmer" />
    </div>
  );
}

/** Highlight/Markdown throws become a readable error instead of a blank pane. */
export function fileExplorerCatchRender<T>(run: () => T): { value: T | null; error: string | null } {
  try {
    return { value: run(), error: null };
  } catch (error) {
    return { value: null, error: errorMessage(error) };
  }
}

/** Line number carried by the nearest ancestor element with a `data-line`
 *  attribute. Source rows stamp their ACTUAL file line (startCursor-based),
 *  never a diff row index. Text nodes climb to their parent; unnumbered
 *  nodes → null. */
export function fileExplorerLineDataOfNode(node: unknown): number | null {
  let current: unknown = node;
  for (let depth = 0; depth < 64 && current != null && typeof current === 'object'; depth += 1) {
    const element = current as {
      hasAttribute?: (name: string) => boolean;
      getAttribute?: (name: string) => string | null;
      parentNode?: unknown;
    };
    if (typeof element.hasAttribute === 'function' && element.hasAttribute('data-line')) {
      const raw = typeof element.getAttribute === 'function' ? element.getAttribute('data-line') : null;
      const line = raw == null ? Number.NaN : Number(raw);
      return Number.isInteger(line) && line >= 1 ? line : null;
    }
    current = element.parentNode;
  }
  return null;
}

/** Ordered validated range from the two selection endpoints' line numbers.
 *  Either endpoint outside numbered source rows → null: never a partial
 *  range, never diff indices. */
export function fileExplorerSelectionRange(
  anchorLine: number | null,
  focusLine: number | null,
): FileConversationLineRange | null {
  if (anchorLine == null || focusLine == null) return null;
  if (!Number.isInteger(anchorLine) || !Number.isInteger(focusLine)) return null;
  if (anchorLine < 1 || focusLine < 1) return null;
  return { startLine: Math.min(anchorLine, focusLine), endLine: Math.max(anchorLine, focusLine) };
}

/** Selection range from a DOM Selection-like pair; the Ask send attaches it
 *  to the prompt. */
export function fileExplorerSelectionFromSelection(
  selection: { anchorNode: unknown; focusNode: unknown } | null,
): FileConversationLineRange | null {
  if (!selection) return null;
  return fileExplorerSelectionRange(
    fileExplorerLineDataOfNode(selection.anchorNode),
    fileExplorerLineDataOfNode(selection.focusNode),
  );
}

/** Painted-source overlay only (floating Ask/Blame bar). Owner, path,
 *  file view, and a range fully inside the painted source lines
 *  (startCursor-based, inclusive). A pagination window, compaction, diff,
 *  or other session yields null so the bar hides — it does not own the
 *  retained Ask range. */
export function fileExplorerPaintedSelection(
  stored: { ownerKey: string; path: string; range: FileConversationLineRange } | null,
  scope: {
    ownerKey: string;
    path: string | null;
    view: string;
    file: { kind: string; startCursor: number; lines: readonly unknown[] } | null;
  },
): FileConversationLineRange | null {
  const retained = fileExplorerAskSelectionRange(stored, scope);
  if (retained == null || scope.file == null || scope.file.kind !== 'file') return null;
  const paintedStart = scope.file.startCursor + 1;
  const paintedEnd = scope.file.startCursor + scope.file.lines.length;
  if (retained.startLine < paintedStart || retained.endLine > paintedEnd) return null;
  return retained;
}

/** Owner/path/file-view Ask range. Independent of the painted page window:
 *  cache compaction must not drop a selection the dock still discloses.
 *  The backend reads this exact range; this does not invent one. */
export function fileExplorerAskSelectionRange(
  stored: { ownerKey: string; path: string; range: FileConversationLineRange } | null,
  scope: { ownerKey: string; path: string | null; view: string },
): FileConversationLineRange | null {
  if (stored == null || scope.path == null) return null;
  if (stored.ownerKey !== scope.ownerKey || stored.path !== scope.path) return null;
  if (scope.view !== 'file') return null;
  if (stored.range.startLine > stored.range.endLine) return null;
  return stored.range;
}

/** Snapshot poll cadence: fast while any polled conversation streams. */
export const FILE_CONVERSATION_POLL_IDLE_MS = 1500;
export const FILE_CONVERSATION_POLL_STREAMING_MS = 500;

export function FileExplorerPanel({
  sendCommand,
  sessionId,
  slot,
  hostAuthority,
  pageProtocol,
  downloadOrigin,
  workspaceCwd,
  onOpenPath,
  onClose,
  onPatchSlot,
  onConversationError,
  conversationSessionId,
  markdownRunnerId,
  markdownMappingId,
  connectionEpoch,
  toggleId = FILE_EXPLORER_TOGGLE_ID,
  exclusivePanes = false,
  expanded = false,
  onToggleExpanded,
}: FileExplorerPanelProps) {
  const [changes, setChanges] = useState<FileExplorerChange[]>([]);
  const [changesTruncated, setChangesTruncated] = useState(false);
  const [isRepo, setIsRepo] = useState(true);
  const [changesError, setChangesError] = useState<string | null>(null);
  const [changesLoading, setChangesLoading] = useState(false);
  const [changesGeneration, setChangesGeneration] = useState(0);
  const [treeQuery, setTreeQuery] = useState('');
  const treeNeedle = fileExplorerNormalizeTreeQuery(treeQuery);
  const dirCache = useSyncExternalStore(
    subscribeFileExplorerDirCache,
    getFileExplorerDirCacheSnapshot,
    getFileExplorerDirCacheSnapshot,
  );
  const dirs = fileExplorerDirsForSession(dirCache, sessionId);
  /** Panel root section. The document keydown effect resolves its Escape
   *  focus owner through it: the own-route Files toggle lookup. */
  const rootRef = useRef<HTMLElement | null>(null);
  // View geometry is a module external store keyed by this panel's owner
  // key (App: sessionId; Chat: the mapping id the host passes AS sessionId).
  // A directory toggle or tree scroll writes the store synchronously and
  // rerenders ONLY this panel — App/Chat top-level state is never touched,
  // so the transcript never re-renders for tree geometry.
  const viewOwnerKey = sessionId ?? '';
  const viewCache = useSyncExternalStore(
    subscribeFileExplorerViewCache,
    getFileExplorerViewCacheSnapshot,
    getFileExplorerViewCacheSnapshot,
  );
  const resolvedView = viewCache[viewOwnerKey] ?? emptyFileExplorerViewState();
  const expandedDirs = resolvedView.expandedDirs;
  const treePct = resolvedView.treePct;
  const [treeCollapsed, setTreeCollapsed] = useState(() =>
    Boolean(slot.activePath)
    && (exclusivePanes
      || (typeof window !== 'undefined' && window.matchMedia(FILE_EXPLORER_SPLIT_QUERY).matches)),
  );
  const patchView = useCallback((patch: Partial<FileExplorerViewState>) => {
    patchFileExplorerViewCache(viewOwnerKey, patch);
  }, [viewOwnerKey]);
  // Module page cache is the authority. The panel never mirrors it in
  // local state: useSyncExternalStore re-renders on store updates, so an
  // A settlement cannot be overwritten by a later B local write-back.
  const pageCache = useSyncExternalStore(
    subscribeFileExplorerPageCache,
    getFileExplorerPageCacheSnapshot,
    getFileExplorerPageCacheSnapshot,
  );
  const [downloadError, setDownloadError] = useState<string | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const [downloading, setDownloading] = useState(false);
  const [isNarrow, setIsNarrow] = useState(() =>
    exclusivePanes || (typeof window !== 'undefined' ? window.matchMedia(FILE_EXPLORER_SPLIT_QUERY).matches : false),
  );

  const aliveRef = useRef(true);
  const sessionRef = useRef(sessionId);
  const splitBodyRef = useRef<HTMLDivElement | null>(null);
  const splitDragRef = useRef(false);
  const treeViewportRef = useRef<HTMLDivElement | null>(null);
  const treeScrollRestoredRef = useRef(false);
  const treeReloadGenerationRef = useRef<number | null>(null);
  const markdownRef = useRef<HTMLDivElement | null>(null);
  const codeRef = useRef<HTMLElement | null>(null);
  /** Content identity the cached scroll was last restored for (session,
   * path, view, surface). Pagination appends share the identity and must
   * never re-run the restore write. */
  const contentScrollRestoredKeyRef = useRef<string | null>(null);
  const setCodeRef = useCallback((node: HTMLElement | null) => {
    codeRef.current = node;
  }, []);
  const contentViewportRef = useRef<HTMLDivElement | null>(null);
  const selectionBarRef = useRef<HTMLDivElement | null>(null);
  const blamePopupRef = useRef<HTMLOListElement | null>(null);
  const setBlamePopupRef = useCallback((node: HTMLOListElement | null) => {
    blamePopupRef.current = node;
  }, []);
  const measureSelectionRef = useRef<(() => void) | null>(null);
  const activeTab = slot.tabs.find((tab) => tab.path === slot.activePath) ?? slot.tabs[0];
  const activePath = activeTab?.path ?? null;
  const activeView = activeTab?.view ?? 'file';
  const activeLine = activeTab?.line;
  const fileState = sessionId && activeTab
    ? fileExplorerReadCachedPage(pageCache, sessionId, activeTab.path, activeView)
    : null;
  const truncationInconsistent = fileState != null && fileExplorerTruncationInconsistent(fileState);
  const previewKind = fileState != null && fileState.kind === 'file'
    ? fileExplorerPreviewKind(fileState.path)
    : null;
  const contentSurface = activePath
    ? fileExplorerContentSurface(resolvedView, activePath, activeView)
    : 'source';
  const showPreview = previewKind != null && contentSurface === 'preview';
  const diffRows = useMemo(
    () => fileState?.kind === 'diff' ? parseFileExplorerUnifiedDiff(fileState.lines, fileState.diffStartState) : null,
    [fileState],
  );
  const diffHunks = useMemo(
    () => diffRows ? groupFileExplorerDiffRows(diffRows) : null,
    [diffRows],
  );
  const highlightLanguage = useMemo(() => {
    if (!fileState || fileState.lines.length === 0 || showPreview) return null;
    return diffPathLanguage(fileState.path);
  }, [fileState, showPreview]);
  const highlighted = useMemo(() => {
    if (!fileState || !highlightLanguage || fileState.lines.length === 0) {
      return { fragments: null as Array<string | null> | null, error: null as string | null };
    }
    const texts = diffRows
      ? diffRows.map((row) => row.kind === 'addition' || row.kind === 'deletion' || row.kind === 'context' ? row.text : null)
      : fileState.lines;
    const caught = fileExplorerCatchRender(() => highlightDiffLineFragments(highlightLanguage, texts));
    return { fragments: caught.value, error: caught.error };
  }, [diffRows, fileState, highlightLanguage]);
  const markdownError = useMemo(() => {
    if (!fileState || !showPreview || previewKind !== 'markdown' || fileState.lines.length === 0) return null;
    return fileExplorerCatchRender(() => renderMarkdown(fileState.lines.join('\n'))).error;
  }, [fileState, previewKind, showPreview]);
  const paneKind = fileExplorerContentPaneKind(activePath, fileState);
  const renderError = highlighted.error ?? markdownError;
  const tooLargeError = fileState?.error?.code === 'file_line_too_large';
  const showLines = fileState != null && fileState.lines.length > 0 && !tooLargeError && (paneKind === 'ready' || paneKind === 'error') && !markdownError;
  useEffect(() => {
    if (exclusivePanes) {
      setIsNarrow(true);
      return;
    }
    if (typeof window === 'undefined') return;
    const mq = window.matchMedia(FILE_EXPLORER_SPLIT_QUERY);
    const onChange = (event: MediaQueryListEvent) => setIsNarrow(event.matches);
    setIsNarrow(mq.matches);
    mq.addEventListener('change', onChange);
    return () => mq.removeEventListener('change', onChange);
  }, [exclusivePanes]);
  useEffect(() => {
    if (exclusivePanes && slot.activePath) setTreeCollapsed(true);
  }, [exclusivePanes, slot.activePath]);


  const closeDrawer = useCallback(() => {
    treeScrollRestoredRef.current = false;
    onClose();
  }, [onClose]);

  useEffect(() => {
    aliveRef.current = true;
    sessionRef.current = sessionId;
    treeScrollRestoredRef.current = false;
    return () => {
      aliveRef.current = false;
    };
  }, [sessionId]);

  const currentSession = useCallback((): boolean => {
    return aliveRef.current && sessionRef.current === sessionId;
  }, [sessionId]);
  /** Snapshots per admitted current-tab path. Local and bounded: keys are
   *  pruned to the current slot's tabs (the host caps a session at
   *  FILE_EXPLORER_MAX_OPEN_TABS = 8) and the map clears on owner switch, so
   *  a closed/evicted tab or another session can never keep rendering. */
  const [conversationSnapshots, setConversationSnapshots] = useState<Record<string, FileConversationSnapshot>>({});
  const conversationSnapshotsRef = useRef(conversationSnapshots);
  conversationSnapshotsRef.current = conversationSnapshots;
  const slotRef = useRef(slot);
  slotRef.current = slot;
  /** True while the given tab still exists in the CURRENT slot carrying this
   *  exact conversation id — the settle gate for every async conversation
   *  write, so a closed/rolled-back/retired tab never accepts a late ACK. */
  const conversationTabOwned = useCallback((tabPath: string, conversationId: string): boolean => {
    const current = slotRef.current.tabs.find((candidate) => candidate.path === tabPath);
    return current != null && current.conversationId === conversationId;
  }, []);
  /** Retire one tab's conversation id after the backend conversation is
   *  definitively gone: clears the id only while this tab still carries it
   *  and drops the local snapshot, so the dock resets and polling stops
   *  instead of failing forever. */
  const retireConversationId = useCallback((tabPath: string, conversationId: string) => {
    // Only the CURRENT owner of this exact path+id may retire: a stale
    // not-found for an already-replaced id must not erase the replacement's
    // snapshot or touch the slot.
    if (!conversationTabOwned(tabPath, conversationId)) return;
    setConversationSnapshots((prev) => {
      if (!(tabPath in prev)) return prev;
      const next = { ...prev };
      delete next[tabPath];
      return next;
    });
    onPatchSlot((current) => ({
      ...current,
      tabs: current.tabs.map((candidate) => {
        if (candidate.path !== tabPath || candidate.conversationId !== conversationId) return candidate;
        const next = { ...candidate };
        delete next.conversationId;
        return next;
      }),
    }));
  }, [conversationTabOwned, onPatchSlot]);
  /** Open steps in flight, keyed `path\0conversationId`. A pre-open poll
   *  must never treat the backend's not_found for a not-yet-open id as
   *  retirement — the id only retires once no open for it is pending. */
  const pendingOpensRef = useRef(new Set<string>());
  /** Last applied snapshot write sequence per path. Conversation RPCs are
   *  fenced by their issue sequence: an overlapping (older-issued) poll can
   *  never overwrite a newer prompt/stop ack. */
  const conversationWriteSeqRef = useRef(new Map<string, number>());
  /** Fenced snapshot write: applies only when no NEWER write already landed
   *  for the path. */
  const writeConversationSnapshot = useCallback((tabPath: string, writeSeq: number, snapshot: FileConversationSnapshot): boolean => {
    if (writeSeq < (conversationWriteSeqRef.current.get(tabPath) ?? 0)) return false;
    conversationWriteSeqRef.current.set(tabPath, writeSeq);
    setConversationSnapshots((prev) => (prev[tabPath] === snapshot ? prev : { ...prev, [tabPath]: snapshot }));
    return true;
  }, []);
  /** Poll targets: only tabs of the CURRENT owner carrying a conversation id
   *  (allocated lazily on the first Ask send). */
  const polledTabs = useMemo(
    () => slot.tabs.filter((tab) => typeof tab.conversationId === 'string' && tab.conversationId !== ''),
    [slot.tabs],
  );
  useEffect(() => {
    setConversationSnapshots({});
    setAskSelection(null);
  }, [sessionId]);
  useEffect(() => {
    const admitted = new Set(slot.tabs.map((tab) => tab.path));
    setConversationSnapshots((prev) => {
      if (Object.keys(prev).every((key) => admitted.has(key))) return prev;
      const next: Record<string, FileConversationSnapshot> = {};
      for (const [key, snapshot] of Object.entries(prev)) {
        if (admitted.has(key)) next[key] = snapshot;
      }
      return next;
    });
    // The retained Ask selection is bound to the tab's lifetime: a path
    // that left the slot map (close/eviction) drops its range; reopening
    // starts clean instead of silently reusing the old selection.
    setAskSelection((prev) => (prev != null && !admitted.has(prev.path) ? null : prev));
  }, [slot.tabs]);
  /** Owner/path-scoped retained Ask selection. Captured on source mouseup
   *  AND on document selectionchange (touch long-press/selection-handle
   *  edits, keyboard selection extension) — always BEFORE a composer focus
   *  or Send-click mousedown can repurpose the live selection. A caret
   *  collapsed INSIDE the source pane is an intentional deselection and
   *  clears the range; a collapse elsewhere (composer, tree) retains it.
   *  Non-collapsed selections are stored only when BOTH endpoints sit
   *  inside the active source pane; everything else retains the previous
   *  range. Send uses this retained owner/path range, not the painted
   *  window. */
  const [askSelection, setAskSelection] = useState<{
    ownerKey: string;
    path: string;
    range: FileConversationLineRange;
  } | null>(null);
  const captureAskSelection = useCallback(() => {
    if (!activePath || activeView !== 'file') return;
    const live = typeof window !== 'undefined' && typeof window.getSelection === 'function' ? window.getSelection() : null;
    if (!live) return;
    const root = codeRef.current;
    if (!root) return;
    const insideRoot = root.contains(live.anchorNode) && root.contains(live.focusNode);
    if (live.isCollapsed) {
      if (insideRoot) setAskSelection(null);
      return;
    }
    if (!insideRoot) return;
    const range = fileExplorerSelectionRange(
      fileExplorerLineDataOfNode(live.anchorNode),
      fileExplorerLineDataOfNode(live.focusNode),
    );
    if (!range) return;
    setAskSelection((prev) => (
      prev != null && prev.ownerKey === viewOwnerKey && prev.path === activePath
        && prev.range.startLine === range.startLine && prev.range.endLine === range.endLine
        ? prev
        : { ownerKey: viewOwnerKey, path: activePath, range }
    ));
  }, [activePath, activeView, viewOwnerKey]);
  // selectionchange covers touch long-press/selection-handle edits and
  // keyboard selection extensions that never produce a source mouseup.
  useEffect(() => {
    if (typeof document === 'undefined' || typeof document.addEventListener !== 'function') return;
    document.addEventListener('selectionchange', captureAskSelection);
    return () => document.removeEventListener('selectionchange', captureAskSelection);
  }, [captureAskSelection]);
  useEffect(() => {
    if (!sessionId || polledTabs.length === 0) return;
    let cancelled = false;
    let timerId: number | undefined;
    const pollOnce = () => {
      for (const tab of polledTabs) {
        const conversationId = tab.conversationId;
        if (typeof conversationId !== 'string' || conversationId === '') continue;
        // A not-yet-open (or being-re-opened) id is skipped: the backend
        // not_founds it until the open ACK lands.
        if (pendingOpensRef.current.has(`${tab.path}\0${conversationId}`)) continue;
        const writeSeq = nextFileExplorerRequestGeneration();
        sendCommand(fileExplorerCommandForSession(sessionId, fileConversationSnapshotCommand(conversationId)))
          .then((data) => {
            if (cancelled || !currentSession() || !conversationTabOwned(tab.path, conversationId)) return;
            // Exact expected id+path: a stale or malformed response is
            // rejected whole and reported on the host's error surface;
            // nothing is written.
            const snapshot = normalizeFileConversationSnapshot(data, { conversationId, path: tab.path });
            if (!snapshot) {
              onConversationError('unusable file conversation snapshot response');
              return;
            }
            writeConversationSnapshot(tab.path, writeSeq, snapshot);
          })
          .catch((error: unknown) => {
            if (cancelled || !currentSession() || !conversationTabOwned(tab.path, conversationId)) return;
            if (isTransportError(error)) return;
            if (fileConversationCloseAlreadyRetired(error)) {
              // The conversation is definitively gone. While an open for
              // this id is still pending this is the pre-open race, not a
              // death — skip. Otherwise retire the tab's id (the dock
              // resets and polling stops) instead of failing forever.
              if (!pendingOpensRef.current.has(`${tab.path}\0${conversationId}`)) {
                retireConversationId(tab.path, conversationId);
              }
              return;
            }
            onConversationError(errorMessage(error));
          });
      }
      const streaming = polledTabs.some((tab) => conversationSnapshotsRef.current[tab.path]?.streaming === true);
      timerId = window.setTimeout(pollOnce, streaming ? FILE_CONVERSATION_POLL_STREAMING_MS : FILE_CONVERSATION_POLL_IDLE_MS);
    };
    pollOnce();
    return () => {
      cancelled = true;
      window.clearTimeout(timerId);
    };
  }, [conversationTabOwned, currentSession, onConversationError, polledTabs, retireConversationId, sendCommand, sessionId, writeConversationSnapshot]);

  const loadDir = useCallback((path: string) => {
    if (!sessionId) return;
    const generation = nextFileExplorerRequestGeneration();
    if (path === '' && !treeScrollRestoredRef.current) {
      treeReloadGenerationRef.current = generation;
    }
    updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(
      prev,
      sessionId,
      path,
      (state) => fileExplorerBeginDirState(state, generation),
    ));
    const identity = fileExplorerDirInflightKey(sessionId, path);
    const request = fileExplorerAcquireDirInflight(
      identity,
      () => sendCommand(fileExplorerCommandForSession(
        sessionId,
        fileExplorerListCommand(path === '' ? undefined : path),
      )),
    );
    void request
      .then((data) => {
        if (!currentSession()) return;
        const parsed = normalizeList(data);
        if (!parsed) {
          updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(
            prev,
            sessionId,
            path,
            (state) => fileExplorerFailDirState(state, 'unusable list response', connectionEpoch, generation),
          ));
          return;
        }
        updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(
          prev,
          sessionId,
          path,
          (state) => fileExplorerApplyDirState(state, parsed.entries, parsed.truncated, generation),
        ));
      })
      .catch((error: unknown) => {
        if (!currentSession()) return;
        const stamped = error != null && typeof error === 'object' && 'rpcCode' in error ? error.rpcCode : undefined;
        updateFileExplorerDirCache((prev) => fileExplorerWriteDirState(
          prev,
          sessionId,
          path,
          (state) => fileExplorerFailDirState(
            state,
            isTransportError(error) ? null : errorMessage(error),
            connectionEpoch,
            generation,
            isTransportError(error) ? undefined : rpcErrorCode({ code: stamped }),
          ),
        ));
      });
  }, [connectionEpoch, currentSession, sendCommand, sessionId]);

  const loadChanges = useCallback(() => {
    if (!sessionId) return;
    setChangesGeneration(nextFileExplorerRequestGeneration());
    setChangesLoading(true);
    setChangesError(null);
    sendCommand(fileExplorerCommandForSession(sessionId, fileExplorerChangesCommand()))
      .then((data) => {
        if (!currentSession()) return;
        const parsed = normalizeChanges(data);
        if (!parsed) {
          setChanges([]);
          setChangesTruncated(false);
          setIsRepo(false);
          setChangesError('unusable changes response');
          setChangesLoading(false);
          return;
        }
        setChanges(parsed.files);
        setChangesTruncated(parsed.truncated);
        setIsRepo(parsed.isRepo);
        setChangesError(null);
        setChangesLoading(false);
      })
      .catch((error: unknown) => {
        if (!currentSession()) return;
        setChangesLoading(false);
        if (isTransportError(error)) return;
        setChangesError(errorMessage(error));
      });
  }, [currentSession, sendCommand, sessionId]);

  useEffect(() => {
    if (!sessionId) return;
    loadChanges();
    const paths = Object.keys(expandedDirs);
    if (!paths.includes('')) paths.unshift('');
    for (const path of paths) {
      if (dirs[path]?.transportIncomplete) continue;
      loadDir(path);
    }
    // Same-epoch transport failures remain on their visible Retry surface.
    // A later authoritative epoch is owned by the retry effect below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [loadChanges, loadDir, sessionId]);

  const loadActive = useCallback((tab: FileExplorerTab, kind: FileExplorerTab['view'], cursor?: number) => {
    if (!sessionId) return;
    const identity = fileExplorerInflightKey(sessionId, tab.path, kind, cursor);
    const command = kind === 'diff'
      ? fileExplorerDiffCommand(tab.path, cursor, FILE_EXPLORER_DEFAULT_MAX_LINES)
      : fileExplorerReadCommand(tab.path, cursor, FILE_EXPLORER_DEFAULT_MAX_LINES);
    fileExplorerAcquireInflight(identity, () => {
      const request = {
        sessionId,
        path: tab.path,
        view: kind,
        cursor,
        generation: nextFileExplorerRequestGeneration(),
        connectionEpoch,
      };
      updateFileExplorerPageCache((prev) => fileExplorerBeginCachedPage(prev, request));
      const promise = sendCommand(fileExplorerCommandForSession(sessionId, command))
        .then((data) => fileExplorerSettleInflightCache(request, data))
        .catch((error: unknown) => fileExplorerSettleInflightCache(request, null, error))
        .finally(() => fileExplorerReleaseInflight(identity));
      return { request, promise };
    });
  }, [connectionEpoch, sendCommand, sessionId]);
  const refreshExplorer = useCallback(() => {
    if (!sessionId) return;
    loadChanges();
    const paths = Object.keys(expandedDirs);
    if (!paths.includes('')) paths.unshift('');
    for (const path of paths) loadDir(path);
    if (!activeTab) return;
    const fallback = activeView === 'file'
      ? fileExplorerCursorForLine(activeTab.line, FILE_EXPLORER_DEFAULT_MAX_LINES)
      : undefined;
    loadActive(activeTab, activeView, fallback);
  }, [activeTab, activeView, expandedDirs, loadActive, loadChanges, loadDir, sessionId]);
  useEffect(() => {
    if (!sessionId || !activeTab) return;
    const cursor = activeView === 'file'
      ? fileExplorerCursorForLine(activeTab.line, FILE_EXPLORER_DEFAULT_MAX_LINES)
      : undefined;
    const cached = fileExplorerReadCachedPage(pageCache, sessionId, activeTab.path, activeView);
    if (!fileExplorerShouldFetchPage(cached, {
      sessionId,
      path: activeTab.path,
      view: activeView,
      cursor,
    })) return;
    loadActive(activeTab, activeView, cursor);
    // pageCache is read for the active identity only; listing it would
    // refetch after every apply. Tab/session/view changes re-run this.
  }, [activeTab, activeView, loadActive, sessionId]);

  // One retry per new connectionEpoch: transport-stuck directory listings
  // reload, and the mounted active tab read retries the failed cursor
  // exactly once. Comparison is against transportFailedEpoch on the
  // module/dir state — close/unmount/reopen after reconnect still retries.
  useEffect(() => {
    if (!sessionId) return;
    if (fileExplorerShouldRetryTransportDirs(dirs, connectionEpoch)) {
      for (const path of fileExplorerRetryableTransportDirPaths(dirs, connectionEpoch)) loadDir(path);
    }
    if (!activeTab) return;
    const cached = fileExplorerReadCachedPage(pageCache, sessionId, activeTab.path, activeView);
    if (!fileExplorerShouldRetryTransportPage(cached, connectionEpoch)) return;
    const fallback = activeView === 'file'
      ? fileExplorerCursorForLine(activeTab.line, FILE_EXPLORER_DEFAULT_MAX_LINES)
      : undefined;
    loadActive(activeTab, activeView, fileExplorerTransportRetryCursor(cached, fallback));
  }, [activeTab, activeView, connectionEpoch, dirs, loadActive, loadDir, pageCache, sessionId]);

  useEffect(() => {
    if (!activePath || activeTab?.lineRequest == null) return;
    updateFileExplorerViewCache(viewOwnerKey, (view) => fileExplorerWithPathSurface(view, activePath, 'source'));
  }, [activePath, activeTab?.lineRequest, viewOwnerKey]);
  useEffect(() => {
    const requestedLine = activeTab?.lineRequest;
    if (!fileState || requestedLine == null || fileState.loading || contentSurface !== 'source') return;
    // Consume the request only after its Source row is mounted. Preview
    // cannot satisfy a line anchor, and scroll-cache updates must not
    // re-center a request already consumed.
    const node = rootRef.current?.querySelector<HTMLElement>(`[id="${fileExplorerLineId(toggleId, requestedLine)}"]`);
    if (!node) return;
    onPatchSlot((slot) => ({
      ...slot,
      tabs: slot.tabs.map((tab) => (
        tab.path === activePath && tab.lineRequest === requestedLine
          ? { ...tab, lineRequest: undefined }
          : tab
      )),
    }));
    node.scrollIntoView({ block: 'center' });
  }, [activePath, activeTab?.lineRequest, contentSurface, fileState, onPatchSlot, toggleId]);

  // Restore the cached scroll ONCE per content identity. The effect used to
  // list fileState?.lines.length in its deps, so every infinite-scroll page
  // append re-ran it and re-applied the page-cache scrollTop saved by
  // onContentScroll at the previous scroll event — stale by whatever the
  // (touch) momentum advanced during the settle's re-render. Each load-more
  // then yanked the viewport BACKWARD (observed 43–60px per page in a
  // synthetic 60Hz harness; a full momentum stride on a real phone).
  // Appending below the viewport never needs a restore; only a content
  // identity change (tab/file/view/surface) does.
  useEffect(() => {
    const node = showPreview ? markdownRef.current : codeRef.current;
    if (!node || !fileState || fileState.loading) return;
    if (!fileExplorerShouldRestoreFileScroll(activeLine)) return;
    const restoredKey = `${sessionId ?? ''}\0${activePath ?? ''}\0${activeView}\0${showPreview}\0${contentSurface}`;
    if (contentScrollRestoredKeyRef.current === restoredKey) return;
    contentScrollRestoredKeyRef.current = restoredKey;
    node.scrollTop = fileState.scrollTop ?? 0;
  }, [activePath, activeView, activeLine, contentSurface, fileState, showPreview, sessionId]);

  useEffect(() => {
    const node = treeViewportRef.current;
    if (!node) return;
    const root = dirs[''];
    if (!root || root.loading) return;
    const reloadGeneration = treeReloadGenerationRef.current;
    if (reloadGeneration != null && root.generation !== reloadGeneration) return;
    if (treeScrollRestoredRef.current) return;
    node.scrollTop = resolvedView.treeScrollTop;
    treeReloadGenerationRef.current = null;
    treeScrollRestoredRef.current = true;
  }, [dirs, resolvedView.treeScrollTop]);

  // Hot path: the store write is SYNCHRONOUS and FIRST — the twisty/child
  // commit renders from this store on the next frame without waiting for
  // the listing, and no App/Chat setState participates. loadDir starts only
  // after the write (one RPC on first expansion; collapse/re-expand of a
  // settled dir is zero-RPC).
  const toggleDir = useCallback((path: string) => {
    let expanded = false;
    updateFileExplorerViewCache(viewOwnerKey, (view) => {
      const toggled = fileExplorerToggleExpandedDir(view, path);
      expanded = toggled.expanded;
      return toggled.view;
    });
    if (expanded && path !== '' && !dirs[path]) loadDir(path);
  }, [dirs, loadDir, viewOwnerKey]);

  const openDiff = (path: string) => {
    if (isNarrow) setTreeCollapsed(true);
    onOpenPath(path, undefined, 'diff');
  };
  const openFile = (path: string) => {
    if (isNarrow) setTreeCollapsed(true);
    onOpenPath(path, undefined, 'file');
  };

  const downloadPath = useCallback((path: string) => {
    if (!sessionId || downloading) return;
    setDownloading(true);
    setDownloadError(null);
    sendCommand(fileExplorerCommandForSession(sessionId, fileExplorerStageDownloadCommand(path)))
      .then((data) => {
        if (!currentSession()) return;
        const parsed = normalizeStageDownload(data);
        setDownloading(false);
        if (!parsed) {
          setDownloadError('unusable download response');
          return;
        }
        triggerFileExplorerDownload(parsed.downloadId, parsed.fileName, {
          downloadOrigin: downloadOrigin ?? `${pageProtocol}//${hostAuthority}`,
        });
      })
      .catch((error: unknown) => {
        const settled = fileExplorerDownloadSettledError(currentSession(), error);
        if (settled === null) return;
        setDownloading(false);
        setDownloadError(settled);
      });
  }, [currentSession, downloadOrigin, downloading, hostAuthority, pageProtocol, sendCommand, sessionId]);

  const deletePath = useCallback((path: string) => {
    if (!sessionId) return;
    setDeleteError(null);
    sendCommand(fileExplorerCommandForSession(sessionId, { type: 'file_explorer_delete', path }))
      .then(() => {
        if (!currentSession()) return;
        const parentDir = path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '';
        loadDir(parentDir);
        loadDir('');
      })
      .catch((error: unknown) => {
        const settled = fileExplorerDownloadSettledError(currentSession(), error);
        if (settled === null) return;
        setDeleteError(settled);
      });
  }, [currentSession, loadDir, sendCommand, sessionId]);

  // Ask composer draft: existing per-path view-store geometry, so a
  // keystroke never touches App/Chat state and the slot sync prunes a
  // dropped tab's draft.
  const activeDraft = activePath ? fileExplorerConversationDraft(resolvedView, activePath) : '';
  const setAskDraft = useCallback((draft: string) => {
    if (!activePath) return;
    updateFileExplorerViewCache(viewOwnerKey, (view) => fileExplorerWithConversationDraft(view, activePath, draft));
  }, [activePath, viewOwnerKey]);

  const toggleAskSurface = useCallback(() => {
    if (!activePath) return;
    const enteringAsk = contentSurface !== 'ask';
    if (enteringAsk && (rootRef.current?.querySelector('.file-explorer__content-viewport')?.clientWidth ?? 0) < 320) {
      setTreeCollapsed(true);
    }
    updateFileExplorerViewCache(viewOwnerKey, (view) => fileExplorerWithPathSurface(
      view,
      activePath,
      enteringAsk ? 'ask' : 'source',
    ));
  }, [activePath, contentSurface, viewOwnerKey]);

  const openAskSurface = useCallback(() => {
    if (!activePath) return;
    if ((rootRef.current?.querySelector('.file-explorer__content-viewport')?.clientWidth ?? 0) < 320) {
      setTreeCollapsed(true);
    }
    updateFileExplorerViewCache(viewOwnerKey, (view) => fileExplorerWithPathSurface(view, activePath, 'ask'));
  }, [activePath, viewOwnerKey]);

  const focusAskComposer = useCallback(() => {
    const focus = () => {
      const input = rootRef.current?.querySelector('.file-conversation__input');
      if (input instanceof HTMLElement) input.focus();
    };
    if (typeof requestAnimationFrame === 'function') requestAnimationFrame(focus);
    else focus();
  }, []);

  const askAboutSelection = useCallback(() => {
    openAskSurface();
    focusAskComposer();
  }, [focusAskComposer, openAskSurface]);

  const [blamePending, setBlamePending] = useState(false);
  const [blameResult, setBlameResult] = useState<FileExplorerBlameResult | null>(null);
  const blameRequestRef = useRef(0);
  const askSelectionRef = useRef(askSelection);
  askSelectionRef.current = askSelection;
  const blameScope = `${sessionId ?? ''}\0${activePath ?? ''}`;
  useEffect(() => {
    blameRequestRef.current += 1;
    setBlamePending(false);
    setBlameResult(null);
  }, [blameScope]);
  const [mapResult, setMapResult] = useState<FileExplorerMapResult | null>(null);
  const [mapPending, setMapPending] = useState(false);
  const mapRequestRef = useRef(0);
  const mapScope = `${viewOwnerKey}\0${activePath ?? ''}\0${activeView}`;
  useEffect(() => {
    mapRequestRef.current += 1;
    setMapPending(false);
    setMapResult(null);
  }, [mapScope]);
  const requestMap = useCallback(() => {
    if (!sessionId || !activePath || activeView !== 'file') return;
    const generation = nextFileExplorerRequestGeneration();
    mapRequestRef.current = generation;
    const ownerKey = viewOwnerKey;
    const path = activePath;
    const request: FileExplorerMapRequest = { ownerKey, path, generation };
    const stillRendered = () => currentSession() && fileExplorerMapResponseCurrent(request, {
      ownerKey,
      path,
      generation: mapRequestRef.current,
    });
    setMapPending(true);
    setMapResult(null);
    sendCommand(fileExplorerCommandForSession(sessionId, fileExplorerMapCommand(path)))
      .then((data) => {
        if (!stillRendered()) return;
        const parsed = normalizeFileExplorerMap(data, path);
        if (!parsed) {
          onConversationError('unusable file map response');
          return;
        }
        setMapResult(parsed);
      })
      .catch((error: unknown) => {
        if (!stillRendered()) return;
        onConversationError(errorMessage(error));
      })
      .finally(() => {
        if (mapRequestRef.current === generation) setMapPending(false);
      });
  }, [activePath, activeView, currentSession, onConversationError, sendCommand, sessionId, viewOwnerKey]);
  const dismissSelection = useCallback(() => {
    // Escape triage: report whether a selection/blame existed so the document
    // keydown consumes Escape only for a real dismissal and never closes the
    // drawer in the same keystroke.
    const hadSelection = askSelectionRef.current != null;
    blameRequestRef.current += 1;
    setBlamePending(false);
    setBlameResult(null);
    setAskSelection(null);
    const selection = window.getSelection();
    if (selection?.anchorNode && codeRef.current?.contains(selection.anchorNode)) {
      selection.removeAllRanges();
    }
    // Return focus to the source so keyboard dismissal never strands focus
    // on <body> once the bar/blame unmounts.
    if (hadSelection) codeRef.current?.focus();
    return hadSelection;
  }, []);
  const requestBlame = useCallback((range: FileConversationLineRange) => {
    if (!sessionId || !activePath || blamePending) return;
    const requestId = blameRequestRef.current + 1;
    blameRequestRef.current = requestId;
    const path = activePath;
    const ownerKey = viewOwnerKey;
    setBlamePending(true);
    setBlameResult(null);
    sendCommand(fileExplorerCommandForSession(
      sessionId,
      fileExplorerBlameCommand(path, range.startLine, range.endLine),
    ))
      .then((data) => {
        if (blameRequestRef.current !== requestId || !currentSession()) return;
        const live = askSelectionRef.current;
        if (
          live == null
          || live.ownerKey !== ownerKey
          || live.path !== path
          || live.range.startLine !== range.startLine
          || live.range.endLine !== range.endLine
        ) return;
        const parsed = normalizeBlame(data, { path, startLine: range.startLine, endLine: range.endLine });
        if (!parsed || !fileExplorerSamePath(parsed.path, path)) {
          onConversationError('unusable file blame response');
          return;
        }
        setBlameResult(parsed);
      })
      .catch((error: unknown) => {
        if (blameRequestRef.current !== requestId || !currentSession()) return;
        const live = askSelectionRef.current;
        if (
          live == null
          || live.ownerKey !== ownerKey
          || live.path !== path
          || live.range.startLine !== range.startLine
          || live.range.endLine !== range.endLine
        ) return;
        onConversationError(errorMessage(error));
      })
      .finally(() => {
        if (blameRequestRef.current === requestId) setBlamePending(false);
      });
  }, [activePath, blamePending, currentSession, onConversationError, sendCommand, sessionId, viewOwnerKey]);

  /** Ask send: allocates a conversation id ON the active tab on first use
   *  (host slot patch), opens the backend conversation for the exact id+path
   *  (an unconfirmed existing id re-opens IDEMPOTENTLY with the SAME id — a
   *  previously ambiguous open must not be skipped forever; a confirmed
   *  conversation prompts directly), then prompts with the captured owner
   *  and source selection. The returned promise spans the ACTUAL
   *  open+prompt lifetime, so the dock's sending guard holds for the real
   *  duration. Open and prompt ACKs are strict-parsed (exact id+path)
   *  before any state write; every settle re-checks that the tab still
   *  exists in the current slot carrying this exact id. Only an explicitly
   *  accepted prompt ack clears the draft, and only when it is unchanged
   *  since send. */
  const sendFileQuestion = useCallback((): Promise<boolean> => {
    if (!sessionId || !activePath || !activeTab) return Promise.resolve(false);
    const message = fileExplorerConversationDraft(resolvedView, activePath);
    if (message.trim() === '') return Promise.resolve(false);
    if (fileConversationPromptTooLarge(message)) {
      onConversationError(`File question exceeds the ${FILE_CONVERSATION_PROMPT_MAX_BYTES}-byte limit; shorten it and send again`);
      return Promise.resolve(false);
    }
    const tab = activeTab;
    const selection = fileExplorerAskSelectionRange(askSelection, {
      ownerKey: viewOwnerKey,
      path: activePath,
      view: activeView,
    });
    const existingId = typeof tab.conversationId === 'string' && tab.conversationId !== '' ? tab.conversationId : null;
    const allocate = existingId == null;
    const id = existingId ?? newFileConversationId();
    const needsOpen = allocate || conversationSnapshotsRef.current[tab.path] == null;
    if (allocate) {
      onPatchSlot((current) => ({
        ...current,
        tabs: current.tabs.map((candidate) => (
          candidate.path === tab.path ? { ...candidate, conversationId: id } : candidate
        )),
      }));
    }
    let openSettled = false;
    const openKey = `${tab.path}\0${id}`;
    if (needsOpen) pendingOpensRef.current.add(openKey);
    const openStep: Promise<unknown> = needsOpen
      ? Promise.resolve(sendCommand(fileExplorerCommandForSession(sessionId, fileConversationOpenCommand(id, tab.path))))
          .finally(() => {
            pendingOpensRef.current.delete(openKey);
          })
      : Promise.resolve(undefined);
    const openWriteSeq = nextFileExplorerRequestGeneration();
    const promptWriteSeq = nextFileExplorerRequestGeneration();
    return openStep
      .then((openAck) => {
        // The open promise RESOLVED: the backend accepted the open and the
        // conversation exists from here on, whatever the ack shape parses
        // into — the id is never rolled back after this point.
        openSettled = true;
        if (!currentSession() || !conversationTabOwned(tab.path, id)) return null;
        // The open ACK is the conversation snapshot (Rust wire: open →
        // snapshot.to_wire_json): strict-parse it BEFORE prompting; a
        // malformed open response aborts the chain with an error instead of
        // prompting blind, while the id stays for an idempotent re-open.
        const snapshot = needsOpen
          ? normalizeFileConversationSnapshot(openAck, { conversationId: id, path: tab.path })
          : null;
        if (needsOpen && !snapshot) {
          throw new Error('unusable file conversation open response');
        }
        if (snapshot) {
          writeConversationSnapshot(tab.path, openWriteSeq, snapshot);
        }
        return sendCommand(fileExplorerCommandForSession(
          sessionId,
          fileConversationPromptCommand(id, message, selection ?? undefined),
        ));
      })
      .then((data) => {
        // null = the tab was retired mid-chain; the host's cleanup owns it.
        if (data === null || !currentSession() || !conversationTabOwned(tab.path, id)) return false;
        // The prompt ACK is snapshot-shaped (identity + entries + accepted).
        const snapshot = normalizeFileConversationSnapshot(data, { conversationId: id, path: tab.path });
        if (!snapshot) {
          onConversationError('unusable file conversation response');
          return false;
        }
        writeConversationSnapshot(tab.path, promptWriteSeq, snapshot);
        // A busy or explicitly rejected ack keeps the typed draft untouched.
        if (!fileConversationPromptAccepted(snapshot)) return false;
        updateFileExplorerViewCache(viewOwnerKey, (view) => {
          if (fileExplorerConversationDraft(view, tab.path) !== message) return view;
          return fileExplorerWithConversationDraft(view, tab.path, '');
        });
        return true;
      })
      .catch((error: unknown) => {
        if (!currentSession() || !conversationTabOwned(tab.path, id)) return false;
        onConversationError(errorMessage(error));
        if (allocate && !openSettled && !isTransportError(error)) {
          // The open was REJECTED definitively (never resolved): the
          // conversation does not exist — retire the unused id so a retry
          // allocates fresh. Everything else keeps the id: a prompt failure
          // after an accepted open (host's committed-slot diff retires it
          // with the tab), a transport-ambiguous open (the retry re-opens
          // idempotently with the same id), and a resolved-but-malformed
          // open ack (the backend opened; parser failure is not proof of
          // non-creation).
          retireConversationId(tab.path, id);
        }
        return false;
      });
  }, [
    activePath,
    activeTab,
    activeView,
    askSelection,
    conversationTabOwned,
    currentSession,
    onConversationError,
    onPatchSlot,
    resolvedView,
    retireConversationId,
    sendCommand,
    sessionId,
    viewOwnerKey,
    writeConversationSnapshot,
  ]);

  /** Stop interrupts the EXACT conversation id of the currently rendered
   *  snapshot (never a newer reallocated id) and applies the abort ACK
   *  immediately, so streaming clears without waiting for the next poll. A
   *  definitive not-found retires the tab's id like the poll path. */
  const stopFileQuestion = useCallback(() => {
    if (!sessionId || !activePath) return;
    const snapshot = conversationSnapshotsRef.current[activePath];
    if (!snapshot) return;
    const conversationId = snapshot.conversationId;
    const writeSeq = nextFileExplorerRequestGeneration();
    sendCommand(fileExplorerCommandForSession(sessionId, fileConversationAbortCommand(conversationId)))
      .then((data) => {
        if (!currentSession() || !conversationTabOwned(activePath, conversationId)) return;
        const settled = normalizeFileConversationSnapshot(data, { conversationId, path: activePath });
        if (!settled) {
          onConversationError('unusable file conversation stop response');
          return;
        }
        writeConversationSnapshot(activePath, writeSeq, settled);
      })
      .catch((error: unknown) => {
        if (!currentSession() || !conversationTabOwned(activePath, conversationId)) return;
        if (isTransportError(error)) return;
        if (fileConversationCloseAlreadyRetired(error)) {
          retireConversationId(activePath, conversationId);
          return;
        }
        onConversationError(errorMessage(error));
      });
  }, [activePath, conversationTabOwned, currentSession, onConversationError, retireConversationId, sendCommand, sessionId, writeConversationSnapshot]);

  const onContentScroll = () => {
    measureSelectionRef.current?.();
    const node = showPreview ? markdownRef.current : codeRef.current;
    if (!node || !sessionId || !activeTab || !fileState) return;
    const top = node.scrollTop;
    updateFileExplorerPageCache((prev) =>
      fileExplorerSetCachedScrollTop(prev, sessionId, activeTab.path, activeView, top),
    );
    if (!fileExplorerShouldLoadNext({
      scrollTop: top,
      clientHeight: node.clientHeight,
      scrollHeight: node.scrollHeight,
      hasNext: fileExplorerCanLoadMore(fileState),
      isLoading: fileState.loading,
    })) return;
    loadActive(activeTab, fileState.kind, fileState.nextCursor);
  };

  // Short viewport / just-settled page: load at most one next page per
  // settled cursor until the content is scrollable or exhausted.
  useEffect(() => {
    if (!activeTab || !fileState || fileState.loading) return;
    if (!fileExplorerCanLoadMore(fileState)) return;
    const node = showPreview ? markdownRef.current : codeRef.current;
    if (!node) return;
    if (!fileExplorerShouldLoadNext({
      scrollTop: node.scrollTop,
      clientHeight: node.clientHeight,
      scrollHeight: node.scrollHeight,
      hasNext: true,
      isLoading: false,
    })) return;
    loadActive(activeTab, fileState.kind, fileState.nextCursor);
  }, [activeTab, fileState?.kind, fileState?.loading, fileState?.nextCursor, fileState?.lines.length, loadActive, showPreview]);

  const applyTreePct = useCallback((next: number) => {
    patchView({ treePct: clampFileExplorerTreePct(next) });
  }, [patchView]);

  const onSplitPointerDown = useCallback((e: ReactPointerEvent<HTMLDivElement>) => {
    if (isNarrow || e.button !== 0) return;
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    splitDragRef.current = true;
    const body = splitBodyRef.current;
    if (!body) return;
    const rect = body.getBoundingClientRect();
    applyTreePct(fileExplorerTreePctFromPointer(e.clientX, rect.left, rect.width));
  }, [applyTreePct, isNarrow]);

  const onSplitPointerMove = useCallback((e: ReactPointerEvent<HTMLDivElement>) => {
    if (!splitDragRef.current) return;
    const body = splitBodyRef.current;
    if (!body) return;
    const rect = body.getBoundingClientRect();
    applyTreePct(fileExplorerTreePctFromPointer(e.clientX, rect.left, rect.width));
  }, [applyTreePct]);

  const onSplitPointerUp = useCallback((e: ReactPointerEvent<HTMLDivElement>) => {
    if (!splitDragRef.current) return;
    splitDragRef.current = false;
    try {
      e.currentTarget.releasePointerCapture(e.pointerId);
    } catch {
      /* already released */
    }
  }, []);

  const onSplitKeyDown = useCallback((e: ReactKeyboardEvent<HTMLDivElement>) => {
    if (isNarrow) return;
    const next = fileExplorerTreePctForKey(treePct, e.key);
    if (next == null) return;
    e.preventDefault();
    applyTreePct(next);
  }, [applyTreePct, isNarrow, treePct]);

  useEffect(() => {
    const onDocKeyDown = (event: KeyboardEvent) => {
      // Focus owner: the toggle is looked up from the panel root by the
      // host's toggle id (Chat passes a distinct one); a null root
      // (SSR/tests) keeps the document fallback.
      const toggle = fileExplorerKeydownScope(rootRef.current)
        ?.querySelector<HTMLElement>(`#${toggleId}`)
        ?? fileExplorerToggleButton(toggleId);
      fileExplorerHandleDocumentKeyDown(event, onClose, toggle, dismissSelection);
    };
    document.addEventListener('keydown', onDocKeyDown);
    return () => document.removeEventListener('keydown', onDocKeyDown);
  }, [dismissSelection, onClose, toggleId]);
  const root = dirs[''] ?? fileExplorerEmptyDirState();
  const treeLoadingIdentity = fileExplorerTreeLoadingIdentity(
    sessionId,
    root,
    changesLoading,
    changesGeneration,
    changes.length,
  );
  const contentLoadingIdentity = fileExplorerContentLoadingIdentity(
    sessionId,
    activePath,
    activeView,
    paneKind,
    fileState?.generation,
  );
  const treeLoadingVisible = useFileExplorerDelayedLoading(treeLoadingIdentity);
  const contentLoadingVisible = useFileExplorerDelayedLoading(contentLoadingIdentity);
  const treePainted = root.entries.length > 0 || changes.length > 0;
  const contentPainted = (fileState?.lines.length ?? 0) > 0;
  const refreshBusy = fileExplorerIsRefreshBusy(
    treePainted,
    contentPainted,
    root.loading || changesLoading,
    fileState?.loading === true,
  );
  const visibleChanges = useMemo(() => changes.slice(0, CHANGES_CAP), [changes]);
  // One index build per changes array; the recursive WorkspaceTree passes it
  // down instead of rescanning 500 changes per visible subtree (measured
  // ~235ms/render without it at the 500-change cap).
  const changeIndex = useMemo(() => fileExplorerBuildChangeIndex(visibleChanges), [visibleChanges]);
  const showChangesTruncated = changesTruncated || changes.length > CHANGES_CAP;
  const retryActive = () => {
    if (!activeTab) return;
    const fallback = activeView === 'file'
      ? fileExplorerCursorForLine(activeTab.line, FILE_EXPLORER_DEFAULT_MAX_LINES)
      : undefined;
    loadActive(activeTab, activeView, fileExplorerTransportRetryCursor(fileState, fallback));
  };

  const headerPreviewKind = activePath && activeView === 'file' ? fileExplorerPreviewKind(activePath) : null;
  const setContentSurface = (surface: FileExplorerContentSurface) => {
    if (!activePath) return;
    updateFileExplorerViewCache(viewOwnerKey, (view) => fileExplorerWithPathSurface(view, activePath, surface));
  };
  const previewText = fileState != null ? fileState.lines.join('\n') : '';
  // Ask attaches the conversation dock to the CURRENT file tab. The dock is
  // presentational: its snapshot comes from the bounded local snapshot map
  // (keyed per current tab path) or a placeholder bound to the tab's lazily
  // allocated id until the first poll lands.
  const askActive = contentSurface === 'ask' && activePath != null && activeTab != null;
  const askDockSnapshot: FileConversationSnapshot | null = askActive && activePath
    ? conversationSnapshots[activePath] ?? {
        conversationId: typeof activeTab?.conversationId === 'string' ? activeTab.conversationId : '',
        path: activePath,
        streaming: false,
        status: 'ready',
        entries: [],
      }
    : null;
  // Owner/path-scoped retained Ask selection: captured before composer
  // focus or a Send-click mousedown can clear the live selection. The dock
  // and the prompt share this range; the painted window only hides the
  // floating bar when those lines are not on screen.
  const askSelectionRange = fileExplorerAskSelectionRange(askSelection, {
    ownerKey: viewOwnerKey,
    path: activePath,
    view: activeView,
  });
  const paintedSelection = fileExplorerPaintedSelection(askSelection, {
    ownerKey: viewOwnerKey,
    path: activePath,
    view: activeView,
    file: fileState,
  });
  const visibleBlame = blameResult != null && paintedSelection != null
    && blameResult.path === activePath
    && blameResult.lines.length === paintedSelection.endLine - paintedSelection.startLine + 1
    && blameResult.lines.every((line, index) => line.currentLine === paintedSelection.startLine + index)
    ? blameResult
    : null;
  // Floating selection bar: out-of-flow mount in the content viewport,
  // anchored to the painted source-line rects so selecting/dismissing never
  // reflows the source. Recomputed on scroll (measureSelectionRef) and on
  // resize (ResizeObserver, disconnected on cleanup); hidden — never
  // cleared — when the selected lines are not painted in the visible source.
  const selectionFloatRange = contentSurface !== 'map' && showLines && !showPreview && !diffHunks ? paintedSelection : null;
  const [selectionAnchor, setSelectionAnchor] = useState<FileExplorerSelectionAnchor>(
    () => ({ visible: false, top: 0, left: 0, placement: 'below' }),
  );
  useEffect(() => {
    const viewportNode = contentViewportRef.current;
    if (!viewportNode) return;
    if (selectionFloatRange == null) {
      setSelectionAnchor((prev) => (prev.visible ? { ...prev, visible: false } : prev));
      return;
    }
    const codeNode = codeRef.current;
    const barNode = selectionBarRef.current;
    if (!codeNode || !barNode) return;
    const range = selectionFloatRange;
    const measure = () => {
      const vpNode = contentViewportRef.current;
      const measuredBar = selectionBarRef.current;
      if (!vpNode || !measuredBar) return;
      const origin = vpNode.getBoundingClientRect();
      const toolbarNode = vpNode.querySelector<HTMLElement>('.file-explorer__content-toolbar');
      // Anchor bounds cover the source area only — below the content
      // toolbar — so the bar/popup can never park over the toggles.
      const sourceTop = toolbarNode != null
        ? toolbarNode.getBoundingClientRect().bottom - origin.top
        : 0;
      let union: FileExplorerSelectionRect | null = null;
      for (let lineNo = range.startLine; lineNo <= range.endLine; lineNo += 1) {
        const row = codeNode.querySelector<HTMLElement>(`#${fileExplorerLineId(toggleId, lineNo)}`);
        if (!row) continue;
        const rect = row.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) continue;
        const next = {
          top: rect.top - origin.top,
          left: rect.left - origin.left,
          width: rect.width,
          height: rect.height,
        };
        union = union == null ? next : {
          top: Math.min(union.top, next.top),
          left: Math.min(union.left, next.left),
          width: Math.max(union.left + union.width, next.left + next.width) - Math.min(union.left, next.left),
          height: Math.max(union.top + union.height, next.top + next.height) - Math.min(union.top, next.top),
        };
      }
      const barRect = measuredBar.getBoundingClientRect();
      const popupNode = blamePopupRef.current;
      const popupRect = popupNode?.getBoundingClientRect();
      const anchor = fileExplorerSelectionAnchor({
        viewport: {
          top: sourceTop,
          left: 0,
          width: vpNode.clientWidth,
          height: vpNode.clientHeight - sourceTop,
        },
        selection: union,
        bar: barRect.width > 0 && barRect.height > 0
          ? { width: barRect.width, height: barRect.height }
          : { width: 0, height: 0 },
        popup: popupRect != null && popupRect.width > 0 && popupRect.height > 0
          ? { width: popupRect.width, height: popupRect.height }
          : undefined,
      });
      setSelectionAnchor((prev) => (
        prev.visible === anchor.visible
        && Math.abs(prev.top - anchor.top) < 0.5
        && Math.abs(prev.left - anchor.left) < 0.5
        && prev.placement === anchor.placement
        && (prev.popup == null) === (anchor.popup == null)
        && (prev.popup == null || anchor.popup == null || (
          Math.abs(prev.popup.top - anchor.popup.top) < 0.5
          && Math.abs(prev.popup.left - anchor.popup.left) < 0.5
          && Math.abs(prev.popup.maxWidth - anchor.popup.maxWidth) < 0.5
          && Math.abs(prev.popup.maxHeight - anchor.popup.maxHeight) < 0.5
        ))
      ) ? prev : anchor);
    };
    measure();
    measureSelectionRef.current = measure;
    if (typeof ResizeObserver !== 'function') return () => { measureSelectionRef.current = null; };
    const observer = new ResizeObserver(measure);
    observer.observe(codeNode);
    observer.observe(viewportNode);
    const trackedBar = selectionBarRef.current;
    if (trackedBar) observer.observe(trackedBar);
    return () => {
      measureSelectionRef.current = null;
      observer.disconnect();
    };
  }, [activePath, contentSurface, selectionFloatRange == null, selectionFloatRange?.startLine, selectionFloatRange?.endLine, toggleId, visibleBlame == null]);
  // The source/preview/diff markup, verbatim. Ask mode re-parents it inside
  // .file-explorer__content-split (desktop: source 62% | dock 38%; the
  // ≤720px sheet hides the source children so the surface radio row is the
  // Source/Ask toggle). No other rendering difference.
  const sourceSurfacePane = (
    <>
      {showLines && showPreview && previewKind === 'markdown' && (
        <div
          ref={markdownRef}
          className="file-explorer__markdown"
          data-view={fileState.kind}
          data-surface="preview"
          onScroll={onContentScroll}
        >
          <MarkdownBody
            className="file-explorer__markdown-body"
            text={previewText}
          />
        </div>
      )}
      {showLines && showPreview && previewKind === 'html' && (
        <div
          ref={markdownRef}
          className="file-explorer__html-preview"
          data-view={fileState.kind}
          data-surface="preview"
          onScroll={onContentScroll}
        >
          <iframe
            className="file-explorer__html-preview-frame"
            title="HTML preview"
            sandbox=""
            referrerPolicy="no-referrer"
            srcDoc={previewText}
          />
        </div>
      )}
      {showLines && showPreview && previewKind === 'svg' && (
        <div
          ref={markdownRef}
          className="file-explorer__svg-preview"
          data-view={fileState.kind}
          data-surface="preview"
          onScroll={onContentScroll}
        >
          <img
            className="file-explorer__svg-preview-img"
            alt=""
            src={`data:image/svg+xml;charset=utf-8,${encodeURIComponent(previewText)}`}
          />
        </div>
      )}
      {showLines && !showPreview && diffHunks && (
        <div
          ref={setCodeRef}
          className="file-explorer__code file-explorer__diff"
          data-view={fileState.kind}
          onScroll={onContentScroll}
        >
          {diffHunks.map((group) => {
            const headerIndex = group.header ? group.startIndex : -1;
            return (
              <section
                key={`${fileState.path}:hunk:${group.startIndex}`}
                className={`file-explorer__hunk${group.header ? '' : ' file-explorer__hunk--prelude'}`}
              >
                {group.header && (
                  <div className="file-explorer__hunk-header">
                    {safeText(group.header.text)}
                  </div>
                )}
                <pre className="file-explorer__hunk-lines">
                  {group.rows.map((row, rowIndex) => {
                    const index = (group.header ? headerIndex + 1 : group.startIndex) + rowIndex;
                    const lineNo = fileState.startCursor + index + 1;
                    return (
                      <FileExplorerLine
                        key={`${fileState.path}:${lineNo}`}
                        toggleId={toggleId}
                        row={row}
                        lineNo={lineNo}
                        fragment={highlighted.fragments?.[index] ?? null}
                        isTarget={activeLine === lineNo}
                        showDiffGutters
                      />
                    );
                  })}
                </pre>
              </section>
            );
          })}
        </div>
      )}
      {showLines && !showPreview && !diffHunks && (
        <div className="file-explorer__source">
          <pre
            ref={setCodeRef}
            className="file-explorer__code"
            data-view={fileState.kind}
            tabIndex={0}
            role="region"
            aria-label="File source"
            onScroll={onContentScroll}
            onMouseUp={captureAskSelection}
          >
            {fileState.lines.map((text, index) => {
              const lineNo = fileState.startCursor + index + 1;
              return (
                <FileExplorerLine
                  key={`${fileState.path}:${lineNo}`}
                  toggleId={toggleId}
                  row={{ kind: 'context', oldNo: lineNo, prefix: '', text }}
                  lineNo={lineNo}
                  fragment={highlighted.fragments?.[index] ?? null}
                  isTarget={activeLine === lineNo}
                  showDiffGutters={false}
                />
              );
            })}
          </pre>
        </div>
      )}
    </>
  );
  const treeToggle = (
    <button
      type="button"
      id={fileExplorerControlId(toggleId, 'tree-toggle')}
      className={`file-explorer__tree-toggle${treeCollapsed ? ' is-active' : ''}`}
      aria-pressed={treeCollapsed}
      title={treeCollapsed ? 'Expand file tree' : 'Collapse file tree'}
      aria-label={treeCollapsed ? 'Expand file tree' : 'Collapse file tree'}
      onClick={() => setTreeCollapsed((prev) => !prev)}
    >
      <svg className="file-explorer__tree-toggle-glyph" viewBox="0 0 16 16" width="16" height="16" aria-hidden="true" focusable="false">
        <path d="M2 3.5h4.2L7.5 5H14v7.5H2z" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinejoin="round" />
        <path d="M5.5 9.2h5" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
      </svg>
      {isNarrow && treeCollapsed && <span className="file-explorer__tree-back-label">Files</span>}
    </button>
  );


  return (
    <section
      ref={rootRef}
      className="panel-drawer file-explorer"
      aria-label="File explorer"
      data-session-id={conversationSessionId ?? sessionId ?? ''}
      data-workspace-cwd={workspaceCwd ?? undefined}
      data-runner-id={markdownRunnerId}
      data-mapping-id={markdownMappingId}
      style={{ ['--file-explorer-tree-pct' as string]: `${treePct}%` }}
    >
      <div className="file-explorer__head" data-testid="file-explorer-header">
        <h2 className="file-explorer__title">Files</h2>
        <div className="file-explorer__actions">
          <button
            type="button"
            id={fileExplorerControlId(toggleId, 'refresh-btn')}
            className={`file-explorer__refresh-btn${refreshBusy ? ' is-refreshing' : ''}`}
            title="Refresh"
            aria-label="Refresh"
            aria-busy={refreshBusy}
            onClick={refreshExplorer}
          >
            <span className="file-explorer__refresh-glyph" aria-hidden="true">⟳</span>
          </button>
          {onToggleExpanded && (
            <button
              type="button"
              id={fileExplorerControlId(toggleId, 'expand-btn')}
              className={`file-explorer__expand-btn${expanded ? ' is-active' : ''}`}
              aria-pressed={expanded}
              title={expanded ? 'Restore file explorer' : 'Expand to the full app area'}
              aria-label={expanded ? 'Restore file explorer' : 'Expand file explorer to the full app area'}
              onClick={onToggleExpanded}
            >
              ⛶
            </button>
          )}
          <button
            type="button"
            id={fileExplorerControlId(toggleId, 'drawer-close-btn')}
            className="file-explorer__close"
            title="Close file explorer"
            aria-label="Close file explorer"
            onClick={closeDrawer}
          >
            {'\u00D7'}
          </button>
        </div>
      </div>
      <div
        ref={splitBodyRef}
        className={`file-explorer__body${treeCollapsed ? ' file-explorer__body--tree-collapsed' : ''}`}
        data-testid="file-explorer-layout"
        data-layout={fileExplorerBodyLayout(isNarrow, treeCollapsed)}
        style={{ ['--file-explorer-tree-pct' as string]: `${treePct}%` }}
      >
        <div
          className="file-explorer__tree-viewport"
          data-testid="file-explorer-tree"
          aria-label="Workspace files"
          role="region"
          hidden={treeCollapsed}
        >
          <div className="file-explorer__tree-toolbar">
            <div className="file-explorer__search">
              <input
                type="search"
                className="file-explorer__search-input"
                value={treeQuery}
                onChange={(event) => setTreeQuery(event.target.value)}
                placeholder="Search files…"
                aria-label="Search workspace files"
                data-testid="file-explorer-search"
              />
            </div>
            {!treeCollapsed && treeToggle}
          </div>
          <div
            ref={treeViewportRef}
            className="file-explorer__tree-list"
            onScroll={() => {
              if (!treeScrollRestoredRef.current) return;
              const top = treeViewportRef.current?.scrollTop ?? 0;
              patchView({ treeScrollTop: top });
            }}
          >
            {changesError && <div className="file-explorer__error">{safeText(changesError)}</div>}
            {!changesLoading && !changesError && !isRepo && <div className="file-explorer__muted">Not a git repository.</div>}
            {(root.error || root.transportIncomplete) && !root.loading && root.errorCode !== 'directory_unavailable' && (
              <div className="file-explorer__error">
                {safeText(root.error ?? 'Connection lost')}
                <button type="button" className="file-explorer__retry" onClick={() => loadDir('')}>Retry</button>
              </div>
            )}
            {downloadError && <div className="file-explorer__error">{safeText(downloadError)}</div>}
            {deleteError && <div className="file-explorer__error">{safeText(deleteError)}</div>}
            <WorkspaceTree
              path=""
              dirs={dirs}
              expandedDirs={expandedDirs}
              onToggleDir={toggleDir}
              onRetryDir={loadDir}
              onOpenFile={openFile}
              onOpenDiff={openDiff}
              onDownload={downloadPath}
              changes={visibleChanges}
              changeIndex={changeIndex}
              activePath={activePath}
              workspaceCwd={workspaceCwd}
              treeQuery={treeNeedle}
              onDeleteFile={deletePath}
            />
            {showChangesTruncated && <div className="file-explorer__muted">Changes truncated (5000-entry cap).</div>}
            {root.truncated && <div className="file-explorer__muted">Listing truncated (5000-entry cap).</div>}
          </div>
          {treeLoadingVisible && <FileExplorerPaneLoading pane="tree" />}
        </div>
        <div
          className="file-explorer__divider"
          data-testid="file-explorer-divider"
          role="separator"
          hidden={treeCollapsed || isNarrow}
          aria-orientation={isNarrow ? 'horizontal' : 'vertical'}
          aria-label={isNarrow || treeCollapsed ? undefined : 'Resize file tree'}
          aria-valuemin={isNarrow ? undefined : FILE_EXPLORER_TREE_PCT_MIN}
          aria-valuemax={isNarrow ? undefined : FILE_EXPLORER_TREE_PCT_MAX}
          aria-valuenow={isNarrow ? undefined : treePct}
          tabIndex={isNarrow ? undefined : 0}
          onPointerDown={isNarrow ? undefined : onSplitPointerDown}
          onPointerMove={isNarrow ? undefined : onSplitPointerMove}
          onPointerUp={isNarrow ? undefined : onSplitPointerUp}
          onPointerCancel={isNarrow ? undefined : onSplitPointerUp}
          onKeyDown={isNarrow ? undefined : onSplitKeyDown}
        />
        <div
          ref={contentViewportRef}
          className="file-explorer__content-viewport"
          data-testid="file-explorer-content"
          aria-label="Selected file"
          role="region"
          hidden={isNarrow && !treeCollapsed}
        >
          <div className="file-explorer__content-toolbar">
            {treeCollapsed && treeToggle}
            {activeTab && activeView === 'file' && (
              <div className="file-explorer__surface" role="radiogroup" aria-label="File surface">
              <button
                type="button"
                className={`file-explorer__surface-btn${contentSurface === 'source' ? ' is-active' : ''}`}
                role="radio"
                aria-checked={contentSurface === 'source'}
                disabled={fileState == null || fileState.lines.length === 0}
                onClick={() => setContentSurface('source')}
              >
                Source
              </button>
              {headerPreviewKind && (
                <button
                  type="button"
                  className={`file-explorer__surface-btn${contentSurface === 'preview' ? ' is-active' : ''}`}
                  role="radio"
                  aria-checked={contentSurface === 'preview'}
                  disabled={fileState == null || fileState.lines.length === 0}
                  onClick={() => setContentSurface('preview')}
                >
                  Preview
                </button>
              )}
              <button
                type="button"
                id={fileExplorerControlId(toggleId, 'ask-toggle')}
                className={`file-explorer__surface-btn file-explorer__ask-toggle${contentSurface === 'ask' ? ' is-active' : ''}`}
                role="radio"
                aria-checked={contentSurface === 'ask'}
                data-testid="file-explorer-ask-toggle"
                title={contentSurface === 'ask' ? 'Hide file conversation' : 'Ask about this file'}
                aria-label={contentSurface === 'ask' ? 'Hide file conversation' : 'Ask about this file'}
                onClick={toggleAskSurface}
              >
                Ask
              </button>
              <button
                type="button"
                id={fileExplorerControlId(toggleId, 'map-toggle')}
                className={`file-explorer__surface-btn${contentSurface === 'map' ? ' is-active' : ''}`}
                role="radio"
                aria-checked={contentSurface === 'map'}
                data-testid="file-explorer-map-toggle"
                title="Dependency map for this file"
                aria-label="Dependency map for this file"
                onClick={() => {
                  setContentSurface('map');
                  requestMap();
                }}
              >
                Map
              </button>
            </div>
            )}
          </div>
          {contentLoadingVisible && <FileExplorerPaneLoading pane="content" />}
          {(paneKind === 'error' || renderError) && (
            <div className={tooLargeError ? 'file-explorer__muted' : 'file-explorer__error'}>
              {tooLargeError
                ? FILE_EXPLORER_TOO_LARGE_MESSAGE
                : safeText(renderError ?? fileState?.error?.message ?? (fileState?.transportIncomplete ? 'Connection lost' : 'render failed'))}
              {!tooLargeError && <button type="button" className="file-explorer__retry" onClick={retryActive}>Retry</button>}
            </div>
          )}
          {fileState?.transportIncomplete && fileState.lines.length > 0 && (
            <div className="file-explorer__error">
              Connection lost
              <button type="button" className="file-explorer__retry" onClick={retryActive}>Retry</button>
            </div>
          )}
          {paneKind === 'empty' && (
            <div className="file-explorer__muted">Empty file</div>
          )}
          {askActive ? (
            <div className="file-explorer__content-split" data-testid="file-explorer-ask-split">
              {sourceSurfacePane}
              <FileConversationDock
                snapshot={askDockSnapshot}
                draft={activeDraft}
                onDraft={setAskDraft}
                onSend={sendFileQuestion}
                onStop={stopFileQuestion}
                onError={onConversationError}
                sessionId={conversationSessionId ?? sessionId}
                cwd={workspaceCwd ?? null}
                selection={askSelectionRange}
              />
            </div>
          ) : contentSurface === 'map' && activePath != null && activeView === 'file' ? (
            <div className="file-explorer__map-pane" data-testid="file-explorer-map-pane">
              {mapPending && <div className="file-explorer__muted">Loading dependency map…</div>}
              {!mapPending && mapResult && fileExplorerSamePath(mapResult.path, activePath) && (
                <FileDependencyMap
                  map={mapResult}
                  workspaceCwd={workspaceCwd}
                  onOpenFile={openFile}
                />
              )}
            </div>
          ) : (
            sourceSurfacePane
          )}
          {truncationInconsistent && (
            <div className="file-explorer__error">Truncated (inconsistent page).</div>
          )}
          {!truncationInconsistent && fileState?.truncated && fileState.nextCursor == null && (
            <div className="file-explorer__muted">Truncated.</div>
          )}
          {selectionFloatRange && (
            <div
              ref={selectionBarRef}
              className={`file-explorer__selection-float${selectionAnchor.visible ? '' : ' is-hidden'}`}
              style={{ top: selectionAnchor.top, left: selectionAnchor.left }}
              data-placement={selectionAnchor.placement}
            >
              <FileSelectionActions
                range={selectionFloatRange}
                pending={blamePending}
                blame={visibleBlame}
                blameRef={setBlamePopupRef}
                blameStyle={selectionAnchor.popup
                  ? {
                      // The popup positions inside the bar; convert the
                      // viewport-origin anchor into bar-relative offsets.
                      top: selectionAnchor.popup.top - selectionAnchor.top,
                      left: selectionAnchor.popup.left - selectionAnchor.left,
                      maxWidth: selectionAnchor.popup.maxWidth,
                      maxHeight: selectionAnchor.popup.maxHeight,
                    }
                  : undefined}
                onAsk={askAboutSelection}
                onBlame={() => requestBlame(selectionFloatRange)}
                onClose={dismissSelection}
              />
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

function FileExplorerLine({
  row,
  lineNo,
  fragment,
  isTarget,
  showDiffGutters,
  toggleId,
}: {
  row: FileExplorerDiffRow;
  lineNo: number;
  fragment: string | null;
  isTarget: boolean;
  showDiffGutters: boolean;
  toggleId: string;
}) {
  return (
    <span
      id={fileExplorerLineId(toggleId, lineNo)}
      data-line={lineNo}
      className={`file-explorer__line file-explorer__line--${row.kind}${isTarget ? ' is-target' : ''}`}
    >
      {showDiffGutters ? (
        <>
          <span className="file-explorer__line-no file-explorer__line-no--old">{row.oldNo ?? ''}</span>
          <span className="file-explorer__line-no file-explorer__line-no--new">{row.newNo ?? ''}</span>
          <span className="file-explorer__line-prefix">{row.prefix}</span>
        </>
      ) : (
        <span className="file-explorer__line-no">{row.oldNo}</span>
      )}
      {fragment !== null ? (
        <span className="file-explorer__line-text" dangerouslySetInnerHTML={{ __html: fragment }} />
      ) : (
        <span className="file-explorer__line-text">{safeText(row.text)}</span>
      )}
    </span>
  );
}

export function ChangeStatusButton({
  change,
  onOpenDiff,
  workspaceCwd,
  disabledReason,
}: {
  change: FileExplorerChange;
  onOpenDiff: (path: string) => void;
  workspaceCwd?: string;
  disabledReason?: string;
}) {
  const displayPath = (path: string) => fileExplorerDisplayPath(path, workspaceCwd);
  const title = disabledReason
    ?? (change.oldPath
      ? `${displayPath(change.oldPath)} → ${displayPath(change.path)}`
      : `Open diff for ${displayPath(change.path)}`);
  return (
    <button
      type="button"
      className={`file-explorer__status file-explorer__status--${change.status}`}
      data-status={change.status}
      aria-label={safeText(disabledReason ?? `Open diff for ${displayPath(change.path)}`)}
      title={safeText(title)}
      disabled={disabledReason != null}
      onClick={disabledReason == null ? () => onOpenDiff(change.path) : undefined}
    >
      {fileExplorerStatusLetter(change.status)}
    </button>
  );
}

function DirStatusBadge({
  tone,
}: {
  tone?: FileExplorerChangeStatus;
}) {
  if (!tone) return null;
  return (
    <span
      className={`file-explorer__status file-explorer__status--${tone}`}
      data-status={tone}
      aria-hidden="true"
    >
      {fileExplorerStatusLetter(tone)}
    </span>
  );
}

function WorkspaceTree({
  path,
  dirs,
  expandedDirs,
  onToggleDir,
  onRetryDir,
  onOpenFile,
  onOpenDiff,
  onDownload,
  onDeleteFile,
  changes,
  changeIndex,
  activePath,
  workspaceCwd,
  treeQuery = '',
  depth = 0,
}: {
  path: string;
  dirs: Record<string, DirNode>;
  expandedDirs: Record<string, true>;
  onToggleDir: (path: string) => void;
  onRetryDir: (path: string) => void;
  onOpenFile: (path: string) => void;
  onOpenDiff: (path: string) => void;
  onDownload: (path: string) => void;
  onDeleteFile: (path: string) => void;
  changes: readonly FileExplorerChange[];
  /** Memoized per-changes-array index — shared by every visible subtree. */
  changeIndex: FileExplorerChangeIndex;
  activePath: string | null;
  workspaceCwd?: string;
  treeQuery?: string;
  depth?: number;
}) {
  const searching = Boolean(treeQuery);
  const node = dirs[path];
  if (!searching && !expandedDirs[path]) return null;
  const unavailable: Record<string, true> = {};
  for (const [dirPath, state] of Object.entries(dirs)) {
    if (state.errorCode === 'directory_unavailable') unavailable[dirPath] = true;
  }
  const rows: FileExplorerTreeRow[] = fileExplorerMergeTree(path, node?.entries ?? [], changes, changeIndex, unavailable)
    .filter((row) => {
      if (!searching) return true;
      if (row.kind === 'dir') {
        return fileExplorerTreeQueryMatches(treeQuery, row.name, row.path)
          || fileExplorerLoadedDescendantMatches(row.path, treeQuery, dirs);
      }
      return fileExplorerTreeQueryMatches(treeQuery, row.name, row.path);
    });
  if (rows.length === 0) {
    if (searching && depth === 0) {
      return <div className="file-explorer__muted">No files match your search.</div>;
    }
    return null;
  }
  return (
    <ul className="file-explorer__tree" data-depth={depth}>
      {rows.map((row) => {
        const tone = fileExplorerTreeTone(row.kind, row.name);
        if (row.kind === 'dir') {
          const descendantMatch = searching && fileExplorerLoadedDescendantMatches(row.path, treeQuery, dirs);
          const open = descendantMatch || Boolean(expandedDirs[row.path]);
          return (
            <li key={row.path} className="file-explorer__dir">
              <div className="file-explorer__item">
                <button
                  type="button"
                  className={`file-explorer__row file-explorer__row--dir${row.changeCount ? ' file-explorer__row--has-status' : ''}`}
                  style={{ paddingLeft: diffTreePaddingLeft(depth) }}
                  onClick={() => onToggleDir(row.path)}
                  onContextMenu={(event) => {
                    event.preventDefault();
                    onDeleteFile(row.path);
                  }}
                  aria-expanded={open}
                  title={safeText(fileExplorerDisplayPath(row.path, workspaceCwd))}
                  data-tone="dir"
                >
                  <span className={`file-explorer__twisty${open ? ' is-open' : ''}`} aria-hidden="true" />
                  <FileTypeIcon tone="dir" expanded={open} />
                  <span className="file-explorer__row-path">{safeText(row.name)}</span>
                </button>
                {row.changeCount ? <DirStatusBadge tone={row.changeTone} /> : null}
              </div>
              {open && (
                <>
                  {(dirs[row.path]?.error || dirs[row.path]?.transportIncomplete) && !dirs[row.path]?.loading && dirs[row.path]?.errorCode !== 'directory_unavailable' && (
                    <div className="file-explorer__error">
                      {safeText(dirs[row.path].error ?? 'Connection lost')}
                      <button type="button" className="file-explorer__retry" onClick={() => onRetryDir(row.path)}>Retry</button>
                    </div>
                  )}
                  <WorkspaceTree
                    path={row.path}
                    dirs={dirs}
                    expandedDirs={expandedDirs}
                    onToggleDir={onToggleDir}
                    onRetryDir={onRetryDir}
                    onOpenFile={onOpenFile}
                    onOpenDiff={onOpenDiff}
                    onDownload={onDownload}
                    onDeleteFile={onDeleteFile}
                    changes={changes}
                    changeIndex={changeIndex}
                    activePath={activePath}
                    workspaceCwd={workspaceCwd}
                    treeQuery={treeQuery}
                    depth={depth + 1}
                  />
                </>
              )}
            </li>
          );
        }
        const selected = row.path === activePath;
        const disabledReason = row.oversized
          ? FILE_EXPLORER_TOO_LARGE_MESSAGE
          : row.sizeUnknown
            ? FILE_EXPLORER_SIZE_UNAVAILABLE_MESSAGE
            : undefined;
        const disabled = disabledReason != null;
        const openRow = disabled ? undefined : () => {
          if (row.gitOnly || row.change) onOpenDiff(row.change?.path ?? row.path);
          else onOpenFile(row.path);
        };
        const rowTitle = disabledReason ?? safeText(fileExplorerDisplayPath(row.path, workspaceCwd));
        return (
          <li key={row.path} className="file-explorer__item">
            <button
              type="button"
              className={`file-explorer__row file-explorer__row--${tone}${selected ? ' file-explorer__row--active' : ''}${row.gitOnly ? ' file-explorer__row--git-only' : ''}${disabled ? ' file-explorer__row--oversized' : ''}${row.change ? ' file-explorer__row--has-status' : ''}`}
              style={{ paddingLeft: diffTreePaddingLeft(depth) }}
              onClick={openRow}
              onContextMenu={disabled ? undefined : (event) => {
                event.preventDefault();
                if (row.gitOnly) return;
                onDeleteFile(row.path);
              }}
              title={rowTitle}
              aria-label={disabledReason ? safeText(`${row.name}. ${disabledReason}`) : undefined}
              disabled={disabled}
              data-git-only={row.gitOnly ? 'true' : undefined}
              data-preview-unavailable={disabled ? 'true' : undefined}
              data-tone={tone}
            >
              <FileTypeIcon tone={tone} />
              <span className="file-explorer__row-path">{safeText(row.name)}</span>
            </button>
            {row.change && <ChangeStatusButton change={row.change} onOpenDiff={onOpenDiff} workspaceCwd={workspaceCwd} disabledReason={disabledReason} />}
          </li>
        );
      })}
    </ul>
  );
}
