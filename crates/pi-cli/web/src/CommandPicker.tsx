import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
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
}: {
  connected: boolean;
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  onSelect: (text: string) => void;
  onSkillSelect: (text: string) => void;
  onError: (message: string) => void;
  getComposerValue: () => string;
  onCatalog: (commands: CommandEntry[]) => void;
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

  // The backend get_commands is the catalog authority; fetch on each open so
  // the menu reflects freshly loaded skills (reload/reconnect picks up new
  // candidates without a stale client cache). Cheap local RPC.
  const load = useCallback(() => {
    if (!connected) return;
    setLoading(true);
    sendCommand({ type: 'get_commands' })
      .then((data) => {
        const catalog = normalizeCommands(data);
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
                {isSkillCandidate(command) ? (
                  <>
                    <span className="command-picker__name">{command.skillName}</span>
                    <span className="command-picker__desc">{command.description}</span>
                  </>
                ) : (
                  <>
                    <span className="command-picker__name">/{command.name}</span>
                    {command.argumentHint && (
                      <span className="command-picker__arg">{command.argumentHint}</span>
                    )}
                    <span className="command-picker__desc">{command.description}</span>
                  </>
                )}
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}
