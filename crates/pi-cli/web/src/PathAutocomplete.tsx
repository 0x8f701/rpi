import { useEffect, useRef, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent, ReactNode } from 'react';

/**
 * Reusable path-completion combobox for the two free local-path inputs
 * (SessionPanel new-session cwd, ChatApp new project cwd).
 *
 * Wire contract (frozen): request `{type:'path_complete',path,kind,limit?}`
 * (callers close over sessionId and augment inside sendComplete); response
 * `{base,prefix,entries:[{name,path,kind}],truncated}` or null when
 * suggestions are unavailable. The component never widens authority: it only
 * relays what the caller's sendComplete returns, one debounced request per
 * settled input value, stale responses dropped by generation counter.
 */

export type PathCompleteKind = 'directory' | 'file' | 'any';

export interface PathCompleteRequest {
  type: 'path_complete';
  path: string;
  kind: PathCompleteKind;
  limit?: number;
  sessionId?: string;
}

export interface PathCompleteEntry {
  name: string;
  path: string;
  kind: 'directory' | 'file';
}

export interface PathCompleteResponse {
  base: string;
  prefix: string;
  entries: PathCompleteEntry[];
  truncated: boolean;
}

export interface PathAutocompleteProps {
  /** DOM id of the input; also namespaces listbox/option ids. */
  id: string;
  value: string;
  onChange: (next: string) => void;
  /** Opaque send (existing sendCommand wrapped); may return null = unavailable. */
  sendComplete: (req: PathCompleteRequest) => Promise<PathCompleteResponse | null>;
  kind: PathCompleteKind;
  placeholder?: string;
  ariaLabel: string;
  disabled?: boolean;
  /** Native HTML required flag forwarded to the underlying input. */
  required?: boolean;
  /** Enter with the menu closed (never fires while suggestions are shown). */
  onEnter?: () => void;
  /** Native autofocus: the input takes focus when it mounts (panel opens).
   *  Deliberately mount-only — later re-renders never steal focus. */
  autoFocus?: boolean;
}

export const PATH_COMPLETE_LIMIT = 100;
export const PATH_COMPLETE_DEBOUNCE_MS = 160;

/** Segment after the final '/': the text completion will replace. */
export function pathPrefixOf(value: string): string {
  const cut = value.lastIndexOf('/');
  return cut < 0 ? value : value.slice(cut + 1);
}

export function nextHighlight(index: number, delta: number, count: number): number {
  if (count <= 0) return -1;
  if (index < 0) return delta > 0 ? 0 : count - 1;
  const next = (index + delta) % count;
  return next < 0 ? next + count : next;
}

export type PathKeyAction =
  | { type: 'move'; delta: 1 | -1 }
  | { type: 'accept' }
  | { type: 'close' }
  | { type: 'submit' }
  | { type: 'none' };

/**
 * Keyboard contract: menu open -> arrows move, Tab/Enter accept (Enter does
 * NOT submit the surrounding form), Escape closes; menu closed -> Enter
 * submits, everything else untouched.
 */
export function pathKeyAction(key: string, menuOpen: boolean): PathKeyAction {
  if (!menuOpen) return key === 'Enter' ? { type: 'submit' } : { type: 'none' };
  switch (key) {
    case 'ArrowDown':
      return { type: 'move', delta: 1 };
    case 'ArrowUp':
      return { type: 'move', delta: -1 };
    case 'Tab':
    case 'Enter':
      return { type: 'accept' };
    case 'Escape':
      return { type: 'close' };
    default:
      return { type: 'none' };
  }
}

/** Directory suggestions stay open for drilling; exact files close the menu. */
export function joinCompletion(entry: PathCompleteEntry): string {
  return entry.kind === 'directory' && !entry.path.endsWith('/') ? `${entry.path}/` : entry.path;
}

export interface PathCompletionMenu {
  entries: PathCompleteEntry[];
  truncated: boolean;
}

interface PathCompletionHost {
  send(req: PathCompleteRequest): Promise<PathCompleteResponse | null>;
  show(menu: PathCompletionMenu): void;
  hide(): void;
}

interface PathCompletionTimers {
  delay(fn: () => void, ms: number): () => void;
}

const realTimers: PathCompletionTimers = {
  delay: (fn, ms) => {
    const handle = setTimeout(fn, ms);
    return () => clearTimeout(handle);
  },
};

/**
 * Debounced, generation-guarded completion fetch. Every request invalidates
 * in-flight ones (stale responses never touch the menu); only the latest
 * input value may open suggestions. Injectable timers keep it testable.
 */
export interface PathCompletionController {
  request(path: string, kind: PathCompleteKind): void;
  cancel(): void;
  dispose(): void;
}

export function createPathCompletionController(
  host: PathCompletionHost,
  timers: PathCompletionTimers = realTimers,
): PathCompletionController {
  let gen = 0;
  let pendingCancel: (() => void) | null = null;
  const invalidate = (): void => {
    gen += 1;
    if (pendingCancel) {
      pendingCancel();
      pendingCancel = null;
    }
  };
  // Escape/blur/blank-input/supersede all funnel here: the pending debounce is
  // dropped, in-flight responses become stale, and the current menu closes —
  // nothing may reopen it except a fresh request.
  const close = (): void => {
    invalidate();
    host.hide();
  };
  return {
    request(path, kind) {
      close(); // old menu closes immediately, before the debounce settles
      if (path.trim() === '') return;
      const requestGen = ++gen;
      pendingCancel = timers.delay(() => {
        pendingCancel = null;
        void Promise.resolve()
          .then(() => host.send({ type: 'path_complete', path, kind, limit: PATH_COMPLETE_LIMIT }))
          .then((res) => {
            if (gen !== requestGen) return; // stale — drop
            if (!res || !Array.isArray(res.entries) || res.entries.length === 0) {
              host.hide();
              return;
            }
            host.show({
              entries: res.entries.slice(0, PATH_COMPLETE_LIMIT),
              truncated: res.truncated === true,
            });
          })
          .catch(() => {
            if (gen === requestGen) host.hide();
          });
      }, PATH_COMPLETE_DEBOUNCE_MS);
    },
    cancel: close,
    dispose() {
      invalidate();
    },
  };
}

const PATH_KIND_LABEL: Record<PathCompleteEntry['kind'], string> = {
  directory: 'dir',
  file: 'file',
};

/** Accessible listbox; options keep focus on the input via mousedown prevention. */
export function PathCompletionListbox({
  listId,
  idBase,
  menu,
  highlight,
  onAccept,
}: {
  listId: string;
  idBase: string;
  menu: PathCompletionMenu;
  highlight: number;
  onAccept: (index: number) => void;
}): ReactNode {
  return (
    <ul id={listId} className="path-autocomplete__menu" role="listbox" aria-label="Path suggestions">
      {menu.entries.map((entry, index) => (
        <li
          key={`${index}:${entry.kind}:${entry.path}`}
          id={`${idBase}-option-${index}`}
          role="option"
          aria-selected={index === highlight}
          className={`path-autocomplete__option${index === highlight ? ' path-autocomplete__option--active' : ''}`}
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => onAccept(index)}
        >
          <span className={`path-autocomplete__kind path-autocomplete__kind--${entry.kind}`}>
            {PATH_KIND_LABEL[entry.kind]}
          </span>
          <span className="path-autocomplete__name">{entry.name}</span>
        </li>
      ))}
      {menu.truncated && (
        <li className="path-autocomplete__more" aria-hidden="true">
          …more — keep typing to narrow
        </li>
      )}
    </ul>
  );
}

export function PathAutocomplete({
  id,
  value,
  onChange,
  sendComplete,
  kind,
  placeholder,
  ariaLabel,
  disabled = false,
  onEnter,
  required = false,
  autoFocus = false,
}: PathAutocompleteProps) {
  const [menu, setMenu] = useState<PathCompletionMenu | null>(null);
  const [highlight, setHighlight] = useState(-1);
  const sendRef = useRef(sendComplete);
  sendRef.current = sendComplete;
  const controllerRef = useRef<PathCompletionController | null>(null);
  if (controllerRef.current === null) {
    controllerRef.current = createPathCompletionController({
      send: (req) => sendRef.current(req),
      show: (next) => {
        setMenu(next);
        setHighlight(0);
      },
      hide: () => {
        setMenu(null);
        setHighlight(-1);
      },
    });
  }

  useEffect(() => {
    const controller = controllerRef.current;
    if (!controller) return;
    if (disabled) {
      controller.cancel();
      return;
    }
    controller.request(value, kind);
  }, [value, kind, disabled]);
  useEffect(() => () => controllerRef.current?.dispose(), []);

  const open = menu !== null && !disabled;
  const listId = `${id}-listbox`;

  const accept = (index: number): void => {
    const entry = menu?.entries[index];
    if (!entry) return;
    onChange(joinCompletion(entry));
    if (entry.kind !== 'directory') {
      setMenu(null);
      setHighlight(-1);
    }
  };

  const onKeyDown = (e: ReactKeyboardEvent<HTMLInputElement>): void => {
    const action = pathKeyAction(e.key, open);
    switch (action.type) {
      case 'move':
        e.preventDefault();
        setHighlight((h) => nextHighlight(h, action.delta, menu?.entries.length ?? 0));
        break;
      case 'accept':
        e.preventDefault();
        accept(highlight >= 0 ? highlight : 0);
        break;
      case 'close':
        e.preventDefault();
        e.stopPropagation();
        controllerRef.current?.cancel();
        break;
      case 'submit':
        onEnter?.();
        break;
      case 'none':
        break;
    }
  };

  return (
    <span className="path-autocomplete">
      <input
        id={id}
        type="text"
        className="path-autocomplete__input"
        role="combobox"
        aria-expanded={open}
        aria-haspopup="listbox"
        aria-controls={open ? listId : undefined}
        aria-activedescendant={open && highlight >= 0 ? `${id}-option-${highlight}` : undefined}
        aria-autocomplete="list"
        placeholder={placeholder}
        aria-label={ariaLabel}
        disabled={disabled}
        value={value}
        required={required}
        autoFocus={autoFocus}
        spellCheck={false}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={onKeyDown}
        onBlur={() => controllerRef.current?.cancel()}
      />
      {open && menu && (
        <PathCompletionListbox
          listId={listId}
          idBase={id}
          menu={menu}
          highlight={highlight}
          onAccept={accept}
        />
      )}
    </span>
  );
}