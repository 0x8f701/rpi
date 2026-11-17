import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type RefObject } from 'react';
import {
  composeCommandText,
  composeSkillCommandText,
  filterCommands,
  filterSupportedCommands,
  isSkillCandidate,
  normalizeCommands,
  pickerIntentFromDraft,
  primaryCommands,
  skillCandidates,
  type CommandEntry,
} from './commands';

export interface CommandPickerProps {
  connected: boolean;
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  onSelect: (text: string) => void;
  onSkillSelect: (text: string) => void;
  onError: (message: string) => void;
  getComposerValue: () => string;
  onCatalog: (commands: CommandEntry[]) => void;
}

/** Shared `get_commands` fetch: the ONE catalog source for the popover picker
 *  and the composer's inline slash autocomplete. Callers narrow with
 *  `filterSupportedCommands` (the popover keeps the full catalog for
 *  `onCatalog`). */
function fetchCommandCatalog(sendCommand: (command: Record<string, unknown>) => Promise<unknown>): Promise<CommandEntry[]> {
  return sendCommand({ type: 'get_commands' }).then(normalizeCommands);
}

export interface SlashIntent {
  mode: 'commands' | 'skills';
  query: string;
}

/** The composer's inline autocomplete is keyed to the WHOLE trimmed draft
 *  being one slash token (never a mid-text caret probe): `/foo` searches
 *  commands (+ skills, mirroring the picker's query behavior), `/skill
 *  [query]` searches skill candidates. Anything else — multiline drafts, a
 *  command's args being typed — closes the popup. */
export function slashIntentFromDraft(draft: string): SlashIntent | null {
  const trimmed = draft.trim();
  if (!trimmed.startsWith('/') || trimmed.includes('\n')) return null;
  const skill = trimmed.match(/^\/skill(?:\s+(.*))?$/i);
  if (skill) return { mode: 'skills', query: (skill[1] ?? '').trim() };
  if (!/\s/.test(trimmed)) return { mode: 'commands', query: trimmed.slice(1) };
  return null;
}

/** Candidate list for a slash intent — the exact list rule the popover uses,
 *  shared here so the inline popup can never drift from the picker. */
function slashCandidates(catalog: CommandEntry[] | null, intent: SlashIntent): CommandEntry[] {
  if (!catalog) return [];
  const primary = primaryCommands(catalog);
  const skills = skillCandidates(catalog);
  if (intent.mode === 'skills') return filterCommands(skills, intent.query);
  return intent.query.trim() === '' ? primary : filterCommands([...primary, ...skills], intent.query);
}

/** Stable identity for an intent: dismissal and active-index resets key on
 *  this, so the popup stays closed for a settled token but re-opens as soon
 *  as the token changes again. */
function slashIntentKey(intent: SlashIntent): string {
  return `${intent.mode}:${intent.query}`;
}

/** Identity-preserving intent update for the uncontrolled main composer: a
 *  keystroke that does not change the slash token keeps the same object so
 *  React bails out instead of re-rendering on every keypress. */
export function mergeSlashIntent(current: SlashIntent | null, next: SlashIntent | null): SlashIntent | null {
  if (current && next && slashIntentKey(current) === slashIntentKey(next)) return current;
  if (current === null && next === null) return current;
  return next;
}

/** Shared presentational row content for the popover list and the composer's
 *  inline autocomplete — one markup, two surfaces. */
function CommandOptionContent({ command }: { command: CommandEntry }) {
  if (isSkillCandidate(command)) {
    return (
      <>
        <span className="command-picker__name">{command.skillName}</span>
        <span className="command-picker__desc">{command.description}</span>
      </>
    );
  }
  return (
    <>
      <span className="command-picker__name">/{command.name}</span>
      {command.argumentHint && (
        <span className="command-picker__arg">{command.argumentHint}</span>
      )}
      <span className="command-picker__desc">{command.description}</span>
    </>
  );
}

/** Inline slash autocomplete popup painted INSIDE the picker container (the
 *  positioned `.command-picker` anchor), reusing the popover/list/option
 *  styles. A dumb renderer: the picker owns the list/active state and this
 *  never takes focus (mousedown is prevented so the composer keeps its
 *  caret); keyboard interaction happens in the picker's capture listener. */
function SlashAutocomplete({ list, activeIndex, onPick }: {
  list: CommandEntry[];
  activeIndex: number;
  onPick: (index: number) => void;
}) {
  if (list.length === 0) return null;
  return (
    <div className="command-picker__popover" role="listbox" aria-label="Slash command suggestions">
      <ul className="command-picker__list">
        {list.map((command, i) => (
          <li
            key={command.name}
            role="option"
            aria-selected={i === activeIndex}
            className={`command-picker__option${i === activeIndex ? ' is-active' : ''}`}
            onMouseDown={(e) => { e.preventDefault(); }}
            onClick={() => onPick(i)}
          >
            <CommandOptionContent command={command} />
          </li>
        ))}
      </ul>
    </div>
  );
}

/** A slash-menu button left of the composer textarea that opens a searchable
 *  popover of the backend `get_commands` catalog. The backend catalog is
 *  authoritative — this component holds no second command list; it only fetch
 *  `get_commands`, normalizes the wire, searches it, and inserts the chosen
 *  draft into the textarea, then focuses it WITHOUT submitting. The user
 *  confirms and Main dispatches.
 *
 *  Two modes share one popover:
 *   - `commands` lists the Web-executable builtins, including the sole Todo
 *     root `/todo`. Selecting the `/skill` parent (or opening the picker while
 *     the composer already holds a `/skill` prefix) drills into `skills` mode.
 *   - `skills` lists every loaded skill candidate (`source === "skill"`) with
 *     its bare name + description and a PERSISTENT instruction line ("Select a
 *     skill, then press Enter to run it") — the picker never auto-submits, so
 *     the Enter-to-run contract is stated inline every time skills mode shows.
 *     Selecting a candidate inserts `/skill <name>` via `onSkillSelect` (which
 *     stages the draft + visible feedback) so Main's submit path dispatches
 *     the typed `skill` RPC. A truly empty catalog ("No skills loaded") tells
 *     the user where skills load from and how to reload, distinct from a query
 *     that merely matched nothing.
 *  Trusted extension commands (`source === "extension"`) appear in the primary
 *  list and submit as RPC `run`. Prompt templates never enter the picker.
 *  Keyboard (Arrow/Enter/Escape/Back), outside-click dismiss, and ARIA listbox
 *  semantics make it usable without a mouse and on mobile. */
export function CommandPicker({
  connected,
  sendCommand,
  onSelect,
  onSkillSelect,
  onError,
  getComposerValue,
  onCatalog,
  onInlineAccept,
  inlineIntent,
  composerInputRef,
  inlineCanAccept,
}: {
  connected: boolean;
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  onSelect: (text: string) => void;
  onSkillSelect: (text: string) => void;
  onError: (message: string) => void;
  getComposerValue: () => string;
  onCatalog: (commands: CommandEntry[]) => void;
  /** Inline autocomplete: the composer passes ONLY the DOM-derived slash
   *  intent (never a draft copy) plus the composer textarea ref; the catalog,
   *  candidate list, highlight, and Enter/Arrow/Escape handling stay inside
   *  this single picker instance — no second fetch, list, or active state. */
  inlineIntent?: SlashIntent | null;
  composerInputRef?: RefObject<HTMLTextAreaElement | null>;
  onInlineAccept?: (text: string) => void;
  /** Composer-provided live gate for the inline Enter pick — reuses the
   *  composer's EXACT submit gates (composition window, paste-enter guard)
   *  instead of a copied clock/IME reimplementation: a guarded Enter is
   *  neither a pick nor a submit, it falls through unprevented to the
   *  composer's own handler. */
  inlineCanAccept?: () => boolean;
}) {
  const [open, setOpen] = useState(false);
  const [mode, setMode] = useState<'commands' | 'skills'>('commands');
  const [query, setQuery] = useState('');
  const [commands, setCommands] = useState<CommandEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [activeIndex, setActiveIndex] = useState(0);
  const containerRef = useRef<HTMLDivElement>(null);
  const btnRef = useRef<HTMLButtonElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const listId = 'command-picker-list';
  // Mirrors `open` for selection idempotence: click is the SOLE chooser (see
  // the option's onMouseDown/onClick contract below), so a single press
  // dispatches exactly one choose. The ref is defense in depth — a stale
  // render or a future event path that could double-fire choose must not
  // insert the draft twice (the first choose flips it and closes the popover,
  // so the trailing call becomes a no-op). Also stays true across the /skill
  // drill (which does NOT close), so the follow-up candidate pick proceeds.
  const openRef = useRef(false);

  // Inline slash autocomplete (composer textarea popup). The composer owns
  // only the DOM-derived `inlineIntent` (never a draft copy); THIS picker
  // keeps the single catalog/list/active: the popup list derives from this
  // component's `commands` catalog with the same rule the popover uses, the
  // highlight reuses `activeIndex`, and Enter/Arrow/Escape are handled by a
  // CAPTURE-phase keydown listener on the composer textarea (element-level,
  // never a document-global) that runs before React's delegated handlers and
  // stopPropagation()s consumed keys so the composer's Enter-submit and
  // Esc-abort never see them.
  const [inlineDismissed, setInlineDismissed] = useState<string | null>(null);
  const inlineList = useMemo(() => {
    if (!inlineIntent) return [];
    return slashCandidates(commands, inlineIntent);
  }, [commands, inlineIntent]);
  const inlineVisible =
    inlineIntent != null
    && !open
    && onInlineAccept !== undefined
    && composerInputRef !== undefined
    && inlineList.length > 0
    && slashIntentKey(inlineIntent) !== inlineDismissed;
  const inlineActive = inlineList.length === 0 ? -1 : Math.min(activeIndex, inlineList.length - 1);
  // Every (re)open of the popup starts on the first (default) candidate.
  const inlineVisibleKey = inlineVisible && inlineIntent ? slashIntentKey(inlineIntent) : '';
  useEffect(() => {
    if (inlineVisibleKey) setActiveIndex(0);
  }, [inlineVisibleKey]);
  // A dismissal suppresses only the CONTINUOUS token it was issued for: once
  // the intent key moves away (a different token, or the draft cleared), the
  // dismissal expires — Esc → clear → retyping the SAME token reopens, while
  // a settled pick (dismissal + settled intent in one commit) stays closed.
  const inlineIntentKeyNow = inlineIntent ? slashIntentKey(inlineIntent) : '';
  useEffect(() => {
    if (inlineDismissed !== null && inlineIntentKeyNow !== inlineDismissed) setInlineDismissed(null);
  }, [inlineIntentKeyNow, inlineDismissed]);

  const acceptInline = useCallback((entry: CommandEntry) => {
    if (!onInlineAccept) return;
    const text = isSkillCandidate(entry) && entry.skillName
      ? composeSkillCommandText(entry.skillName)
      : composeCommandText(entry.name, entry.requiresArguments);
    onInlineAccept(text);
    // The settled draft IS the completed token; dismiss on ITS intent key so
    // the popup stays closed for the completed token yet re-opens as soon as
    // the user keeps typing a different token.
    const settled = slashIntentFromDraft(text);
    setInlineDismissed(settled ? slashIntentKey(settled) : null);
    setActiveIndex(0);
  }, [onInlineAccept]);

  const closeInline = useCallback(() => {
    const input = composerInputRef?.current;
    const intent = input ? slashIntentFromDraft(input.value) : null;
    setInlineDismissed(intent ? slashIntentKey(intent) : null);
  }, [composerInputRef]);

  // Event-time snapshot for the capture listener (render-phase assignment,
  // the repo's standard ref-mirror pattern) so one mount-time listener always
  // sees the current visibility/list/active without re-attaching per render.
  const inlineStateRef = useRef({
    visible: false,
    list: [] as CommandEntry[],
    active: -1,
    canAccept: null as (() => boolean) | null,
  });
  inlineStateRef.current = { visible: inlineVisible, list: inlineList, active: inlineActive, canAccept: inlineCanAccept ?? null };

  useEffect(() => {
    const input = composerInputRef?.current;
    if (!input) return;
    function onComposerKeyDown(event: globalThis.KeyboardEvent) {
      const state = inlineStateRef.current;
      // IME composition (isComposing / keyCode 229) never triggers select.
      if (!state.visible || event.isComposing || event.keyCode === 229) return;
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        event.preventDefault();
        event.stopPropagation();
        setActiveIndex((i) => (event.key === 'ArrowDown'
          ? Math.min(i + 1, state.list.length - 1)
          : Math.max(i - 1, 0)));
      } else if (
        (event.key === 'Enter' || event.key === 'NumpadEnter')
        && !event.shiftKey && !event.altKey && !event.metaKey && !event.ctrlKey
      ) {
        // Enter with candidates CONSUMES the pick and never submits — but only
        // through the composer's live gates: a guarded Enter (composition
        // window, paste-enter guard) falls through unprevented so the
        // composer's own handler applies the same gates (no pick, no submit).
        if (state.canAccept && !state.canAccept()) return;
        event.preventDefault();
        event.stopPropagation();
        const entry = state.list[Math.max(state.active, 0)];
        if (entry) acceptInline(entry);
      } else if (event.key === 'Escape') {
        event.preventDefault();
        event.stopPropagation();
        closeInline();
      }
    }
    input.addEventListener('keydown', onComposerKeyDown, true);
    return () => input.removeEventListener('keydown', onComposerKeyDown, true);
  }, [composerInputRef, acceptInline, closeInline]);

  // The backend get_commands is the catalog authority; fetch on each open so
  // the menu reflects freshly loaded skills (reload/reconnect picks up new
  // candidates without a stale client cache). Cheap local RPC.
  const load = useCallback(() => {
    if (!connected) return;
    setLoading(true);
    fetchCommandCatalog(sendCommand)
      .then((catalog) => {
        onCatalog(catalog);
        setCommands(filterSupportedCommands(catalog));
        setActiveIndex(0);
        setLoading(false);
      })
      .catch((err: unknown) => {
        openRef.current = false;
        setLoading(false);
        setOpen(false);
        onError(`commands failed: ${err instanceof Error ? err.message : String(err)}`);
      });
  }, [connected, sendCommand, onError, onCatalog]);
  // A fresh composer that starts typing a slash token triggers the SAME
  // single `load` the popover uses (never a second catalog request path).
  // Requested at most ONCE: a legitimately empty catalog or a failed fetch
  // must never spin into repeated get_commands (or repeated error toasts) —
  // the inline popup simply shows nothing and Enter keeps the original
  // submit; the explicit popover open remains the refresh path.
  const inlineLoadRequestedRef = useRef(false);
  useEffect(() => {
    if (!inlineIntent || inlineLoadRequestedRef.current || !connected) return;
    if (commands.length > 0 || loading) return;
    inlineLoadRequestedRef.current = true;
    load();
  }, [inlineIntent, commands.length, loading, connected, load]);

  // Opening while the composer holds `/skill [query]` drills straight into
  // skill candidates AND carries the typed tail into the picker search. This
  // makes `/skill research` + Command immediately show matching skills instead
  // of opening an unfiltered list (the composer remains editable until pick).
  const openPicker = useCallback(() => {
    openRef.current = true;
    setOpen(true);
    setActiveIndex(0);
    const intent = pickerIntentFromDraft(getComposerValue() || '');
    setMode(intent.mode);
    setQuery(intent.query);
    load();
  }, [load, getComposerValue]);

  const close = useCallback(() => {
    openRef.current = false;
    setOpen(false);
    setMode('commands');
    setQuery('');
  }, []);

  // The trigger button is a two-state affordance for the popover: a click
  // while CLOSED opens it (re-deriving mode/query from the current composer
  // draft, so `/skill <query>` pre-filters), a click while OPEN closes it.
  // Escape in skills mode only drills BACK — the popover stays open — so the
  // very next trigger click dismisses, and the one after reopens. Keeping the
  // toggle explicit here (instead of an inline ternary) documents that
  // contract: callers must not assume "Command" always opens; when the
  // popover is already open (e.g. after an Escape drill-back) it dismisses.
  // Reopen-after-close re-reads the composer draft, so the prefiltered
  // `/skill <query>` intent survives an Escape-close→Command cycle.
  const togglePicker = useCallback(() => {
    if (open) {
      close();
    } else {
      openPicker();
    }
  }, [open, close, openPicker]);

  const backToCommands = useCallback(() => {
    setMode('commands');
    setQuery('');
    setActiveIndex(0);
    // Re-focus the search box so keyboard nav keeps working after drilling back.
    searchRef.current?.focus();
  }, []);

  const primary = useMemo(() => primaryCommands(commands), [commands]);
  const candidates = useMemo(() => skillCandidates(commands), [commands]);
  const list = useMemo(() => {
    if (mode === 'skills') return filterCommands(candidates, query);
    // Keep the initial menu compact, but once the user types, search the
    // primary commands AND loaded skills. A concrete skill is therefore
    // discoverable directly from the first search box.
    return query.trim() === '' ? primary : filterCommands([...primary, ...candidates], query);
  }, [mode, candidates, primary, query]);
  const safeActive = list.length === 0 ? -1 : Math.min(activeIndex, list.length - 1);

  const choose = useCallback(
    (entry: CommandEntry) => {
      // Called from click (mouse press) and Enter (keyboard) only — never
      // from mousedown (that must not mutate the list mid-gesture). The
      // openRef guard makes a double-fired choose (stale render, future event
      // path) a no-op once the popover has closed. A click-only dispatch
      // (programmatic or assistive tech) has no preceding mousedown and
      // proceeds normally.
      if (!openRef.current) return;
      // Selecting the `/skill` parent in commands mode drills into the loaded
      // skill candidates instead of inserting a bare `/skill ` draft. The user
      // then picks a candidate (or Escape back) — no auto-submit either way.
      if (mode === 'commands' && entry.name === 'skill') {
        setMode('skills');
        setQuery('');
        setActiveIndex(0);
        searchRef.current?.focus();
        return;
      }
      if (isSkillCandidate(entry) && entry.skillName) {
        // Skill candidates go through onSkillSelect (stage + toast in App):
        // insertCommandText would leave the caret at the end with no feedback,
        // and the contract requires a visible "ready — press Enter to run" cue
        // plus the draft selected so a typed replacement is a single keystroke.
        onSkillSelect(composeSkillCommandText(entry.skillName));
      } else {
        onSelect(composeCommandText(entry.name, entry.requiresArguments));
      }
      close();
      // onSkillSelect / onSelect (insertCommandText) already focus the
      // textarea so the user can type the command's argument; do NOT re-focus
      // the trigger button.
    },
    [mode, onSelect, onSkillSelect, close],
  );

  // Focus the search input when the popover opens or drills between modes; restore
  // focus to the trigger button only when the popover closes via keyboard.
  useEffect(() => {
    if (open) searchRef.current?.focus();
  }, [open, mode]);

  // Outside-click dismiss. mousedown (not click) fires before the option's
  // own click would; a press inside the container never dismisses, so the
  // option's mousedown/click selection always runs before any outside blur.
  useEffect(() => {
    if (!open) return;
    function onDown(e: MouseEvent) {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) close();
    }
    document.addEventListener('mousedown', onDown);
    return () => document.removeEventListener('mousedown', onDown);
  }, [open, close]);

  function onKeyDown(e: KeyboardEvent<HTMLInputElement>) {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      if (list.length) setActiveIndex((i) => Math.min(i + 1, list.length - 1));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      if (list.length) setActiveIndex((i) => Math.max(i - 1, 0));
    } else if (e.key === 'Enter') {
      e.preventDefault();
      const pick = list[safeActive];
      if (pick) choose(pick);
    } else if (e.key === 'Escape') {
      e.preventDefault();
      if (mode === 'skills') {
        // Drill-back: Escape in skill-candidate mode returns to the command
        // list instead of closing the popover, mirroring a back button.
        backToCommands();
      } else {
        close();
        btnRef.current?.focus();
      }
    } else if (e.key === 'Backspace' && mode === 'skills' && query === '') {
      // Backspace on an empty skill-mode query returns to the command list —
      // a natural drill-out for keyboard/mobile users.
      e.preventDefault();
      backToCommands();
    }
  }
  const placeholder = mode === 'skills' ? 'Search skills…' : 'Search commands or skills…';
  const ariaLabel = mode === 'skills' ? 'Search skills' : 'Search commands or skills';
  // Skills mode distinguishes a catalog with NOTHING loaded (renders the
  // guidance block below) from a query that matched no loaded skill. Commands
  // mode keeps its generic no-match hint.
  const emptyHint =
    mode === 'skills'
      ? candidates.length === 0
        ? 'No skills loaded'
        : 'No skills match'
      : 'No commands match';

  return (
    <div className="command-picker" ref={containerRef}>
      <button
        ref={btnRef}
        id="command-btn"
        type="button"
        title="Search commands and skills"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={listId}
        disabled={!connected}
        onClick={togglePicker}
      >
        <span aria-hidden="true">/</span>
        <span className="command-btn__label">Command</span>
      </button>
      {!open && inlineVisible && (
        <SlashAutocomplete
          list={inlineList}
          activeIndex={inlineActive}
          onPick={(index) => {
            const entry = inlineList[index];
            if (entry) acceptInline(entry);
          }}
        />
      )}
      {open && (
        <div className="command-picker__popover" role="dialog" aria-label="Slash commands">
          {mode === 'skills' && (
            <div className="command-picker__header">
              <button
                type="button"
                className="command-picker__back"
                aria-label="Back to commands"
                onClick={backToCommands}
              >
                ‹ Commands
              </button>
              <span className="command-picker__title">Skills</span>
            </div>
          )}
          {mode === 'skills' && (
            <div className="command-picker__tip" role="status">
              Select a skill, then press Enter to run it
            </div>
          )}
          <input
            ref={searchRef}
            className="command-picker__search"
            type="text"
            placeholder={placeholder}
            value={query}
            aria-label={ariaLabel}
            aria-controls={listId}
            aria-expanded={open}
            aria-autocomplete="list"
            autoComplete="off"
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={onKeyDown}
          />
          <ul id={listId} className="command-picker__list" role="listbox" aria-label={ariaLabel}>
            {loading && <li className="command-picker__hint" role="status">Loading…</li>}
            {!loading && list.length === 0 && (
              mode === 'skills' && candidates.length === 0 ? (
                // A genuinely empty skill catalog, not a query that matched
                // nothing: tell the user where skills load from (repo-relative
                // project dir + user skills directory, no machine paths) and
                // the reload actions that pick up newly added skills.
                <li className="command-picker__hint command-picker__hint--empty" role="status">
                  <span className="command-picker__hint-title">No skills loaded</span>
                  <span className="command-picker__hint-detail">
                    Skills load from your project&apos;s <code>.pi/skills</code> and your
                    user skills directory when a session starts. Restart the listener or
                    open a new session to reload.
                  </span>
                </li>
              ) : (
                <li className="command-picker__hint">{emptyHint}</li>
              )
            )}
            {!loading && list.map((command, i) => (
              <li
                key={command.name}
                role="option"
                className={`command-picker__option${i === safeActive ? ' is-active' : ''}`}
                aria-selected={i === safeActive}
                data-skill-name={isSkillCandidate(command) ? command.skillName : undefined}
                // A real mouse press fires mousedown then click on the SAME
                // row. Choosing on mousedown is WRONG for the /skill drill: it
                // swaps the list to skills mid-gesture, so the trailing click
                // lands on whatever candidate now occupies that spot and
                // selects it — closing the popover instead of showing Skills.
                // mousedown therefore only preventDefaults (keeps focus in the
                // search box); click is the SOLE chooser, so one press-release
                // dispatches exactly one choose, on the row the user pressed.
                // Programmatic/assistive clicks (no mousedown) go through the
                // same click path. choose's openRef guard stays as defense in
                // depth against any future event-path that could double-fire.
                onMouseDown={(e) => { e.preventDefault(); }}
                onClick={() => choose(command)}
                onMouseEnter={() => setActiveIndex(i)}
              >
                <CommandOptionContent command={command} />
          </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}
