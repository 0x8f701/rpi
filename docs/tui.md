# TUI and keybindings

The TUI is a normal-screen inline terminal interface built with `crossterm` and
`ratatui`. It preserves terminal and tmux scrollback instead of switching to the
alternate screen. It is selected when both stdin and stdout are terminals;
otherwise the CLI uses print mode for positional prompts or the line REPL for a
text session. The dispatch lives in `crates/pi-cli/src/lib.rs:138-169`.

## Layout

```text
 rpi (rs) · provider/model · /cwd
┌─────────────────────────────────────┐
│ Conversation                        │
│ ...                                 │
├─────────────────────────────────────┤
│ Message                             │
│ > type here                         │
├─────────────────────────────────────┤
│ Status line                         │
└─────────────────────────────────────┘
```

The conversation area keeps the most recent 4,000 **transcript entries**;
when the limit is exceeded the oldest entries are dropped (`MAX_TRANSCRIPT_LINES = 4_000` in `crates/pi-cli/src/tui.rs:58`, trimmed in `tui.rs:1703`).
Tool execution is shown as status lines (`· toolName(args)` / `  └ ok` /
`  └ error`).

When the transcript is empty, the welcome screen lists a few **Recent sessions**
from the unified catalog, scoped to the current working directory. Each entry
shows a `[source]` badge for native Pi, OMP, Codex, Claude, Grok/Hyper, or Droid
sessions. Selecting a foreign session imports it once; subsequent resumes reuse
the converted file under the effective native session root.

## Built-in themes

Only two built-in palettes are always available:

- `dark` — the exact installed OMP v17.2.6 default `titanium` palette: electric blue chrome, green success/readouts, gold highlights, and dark titanium surfaces (`DARK` in `crates/pi-cli/src/theme.rs`).
- `light` — a high-contrast palette for light terminals (`LIGHT` in `theme.rs`).

The initial theme follows safe terminal background detection; dark terminals therefore use the OMP-default Titanium palette. It can be pinned in `settings.json`:

```json
{
  "theme": "light"
}
```

Switch themes in the TUI with `/theme`, `/theme next`, `/theme prev`, or
`/theme <name>`. The theme manager also live-reloads custom theme files and
leaves the active palette unchanged when a reload fails (`theme.rs:353`).

## Custom themes

Place JSON theme files in one of the theme directories:

- Global: `<pi-dir>/themes/` (the `.pi` directory under the user's home directory:
  `HOME` on Unix, `USERPROFILE` on Windows, resolved by `home_dir()` in
  `crates/pi-cli/src/tui.rs:4985`)
- Project: `<workspace>/.pi/themes/`

Directory order is global-first, project-last (`config_paths` in `crates/pi-cli/src/tui.rs:4971`).

A theme file extends `dark` or `light` and overrides only the colors it
specifies:

```json
{
  "name": "solarized",
  "extends": "dark",
  "vars": {
    "blue": "#268bd2",
    "green": "#859900"
  },
  "colors": {
    "accent": "blue",
    "success": "green",
    "selectedBg": "#073642",
    "mdHeading": "#b58900",
    "syntaxKeyword": "blue",
    "toolDiffAdded": "green"
  }
}
```

`extends` must be `"dark"` or `"light"` (`theme.rs:522`). Role names may be
camelCase (upstream) or snake_case (Rust-native); underscores and hyphens are
ignored (`theme.rs:567`).

Semantic roles cover the complete transcript and editor surface (defined on
`Theme` in `theme.rs:41`):

- Core: `accent`, `border`, `borderAccent`, `borderMuted`, `success`, `error`,
  `warning`, `muted`, `dim`, `text`, `thinkingText`
- Messages and tools: `userMessageText`, `customMessageText`,
  `customMessageLabel`, `toolTitle`, `toolOutput`
- Markdown: `mdHeading`, `mdLink`, `mdLinkUrl`, `mdCode`, `mdCodeBlock`,
  `mdCodeBlockBorder`, `mdQuote`, `mdQuoteBorder`, `mdHr`, `mdListBullet`
- Diffs and syntax: `toolDiffAdded`, `toolDiffRemoved`, `toolDiffContext`,
  `syntaxComment`, `syntaxKeyword`, `syntaxFunction`, `syntaxVariable`,
  `syntaxString`, `syntaxNumber`, `syntaxType`, `syntaxOperator`,
  `syntaxPunctuation`
- Thinking and modes: `thinkingOff`, `thinkingMinimal`, `thinkingLow`,
  `thinkingMedium`, `thinkingHigh`, `thinkingXhigh`, `thinkingMax`, `bashMode`
- Backgrounds: `selectedBg`, `userMessageBg`, `customMessageBg`,
  `toolPendingBg`, `toolSuccessBg`, `toolErrorBg`

Color values may be ratatui named colors (`black`, `red`, `cyan`, `gray`,
`darkgray`, `lightgreen`, `white`, `reset`, ...) or hex `#rrggbb` / `#rgb`.
Empty-string values resolve to the terminal default color for `text`,
`userMessageText`, `customMessageText`, and `toolTitle` (`theme.rs:100`).
Theme files are validated before loading; a malformed file is skipped with a
diagnostic and the active theme is left unchanged.

## Keybindings

The TUI maps incoming keys to stable actions through a `KeyBindingsManager`.
Built-in defaults reproduce the original TUI behavior (`default_bindings` in
`crates/pi-cli/src/keybindings.rs:354`). The dispatch layer never checks
hard-coded chords; it resolves every key event to an `Action` (`keybindings.rs:298`).

### Global editor and application defaults

| Chord | Action | Stable ID (config name) |
|-------|--------|-------------------------|
| `enter` | Submit message | `tui.input.submit` |
| `shift+enter`, `ctrl+j` | Insert newline | `tui.input.newLine` |
| `backspace` | Delete character backward | `tui.editor.deleteCharBackward` |
| `delete` | Delete character forward | `tui.editor.deleteCharForward` |
| `left`, `ctrl+b` | Move cursor left | `tui.editor.cursorLeft` |
| `right`, `ctrl+f` | Move cursor right | `tui.editor.cursorRight` |
| `up` | Move cursor / previous history line | `tui.editor.cursorUp` |
| `down` | Move cursor / next history line | `tui.editor.cursorDown` |
| `alt+left`, `ctrl+left`, `alt+b` | Word left | `tui.editor.cursorWordLeft` |
| `alt+right`, `ctrl+right`, `alt+f` | Word right | `tui.editor.cursorWordRight` |
| `home`, `ctrl+a` | Start of line | `tui.editor.cursorLineStart` |
| `end`, `ctrl+e` | End of line | `tui.editor.cursorLineEnd` |
| `ctrl+]` | Jump forward | `tui.editor.jumpForward` |
| `ctrl+alt+]` | Jump backward | `tui.editor.jumpBackward` |
| `pageup` | Scroll transcript up one page | `tui.editor.pageUp` |
| `pagedown` | Scroll transcript down one page | `tui.editor.pageDown` |
| `ctrl+w`, `alt+backspace` | Delete word backward | `tui.editor.deleteWordBackward` |
| `alt+d`, `alt+delete` | Delete word forward | `tui.editor.deleteWordForward` |
| `ctrl+u` | Clear the entire composer | `tui.editor.clear` |
| `ctrl+k` | Delete to line end | `tui.editor.deleteToLineEnd` |
| `ctrl+y` | Yank | `tui.editor.yank` |
| `alt+y` | Yank pop | `tui.editor.yankPop` |
| `ctrl+-` | Undo | `tui.editor.undo` |
| `esc` | Abort in-flight run, or clear input when idle | `app.interrupt` |
| `ctrl+c` | Clear input | `app.clear` |
| `ctrl+d` | Quit (when input is empty) | `app.exit` |
| `tab` | Accept slash-command completion | `tui.input.tab` |
| `ctrl+v`, `alt+v` | Paste from clipboard | `app.clipboard.pasteImage` |
| `ctrl+x`, `ctrl+shift+c` | Copy last assistant message | `app.message.copy` |
| `ctrl+g` | Open external editor | `app.editor.external` |
| `ctrl+z` | Suspend (yield terminal) | `app.suspend` |
| `shift+tab` | Cycle thinking level | `app.thinking.cycle` |
| `ctrl+t` | Toggle thinking block visibility | `app.thinking.toggle` |
| `ctrl+p` | Cycle model forward | `app.model.cycleForward` |
| `ctrl+shift+p` | Cycle model backward | `app.model.cycleBackward` |
| `ctrl+l` | Open model selector | `app.model.select` |
| `ctrl+o` | Expand/collapse tool details | `app.tools.expand` |
| `alt+enter` | Queue follow-up | `app.message.followUp` |
| `alt+up` | Dequeue last prompt | `app.message.dequeue` |

### Contextual selector defaults

Some chords are intentionally reused across mutually exclusive panels; they
only dispatch when the matching panel is open (`resolve_in` in
`keybindings.rs:303`, selector actions in `keybindings.rs:323`).

**Saved session selector** (`/sessions`, `/resume` with no argument):

| Chord | Action |
|-------|--------|
| `ctrl+n` | Toggle named-only filter (`app.session.toggleNamedFilter`) |
| `ctrl+p` | Toggle path display (`app.session.togglePath`) |
| `ctrl+s` | Toggle sort (`app.session.toggleSort`) |
| `ctrl+r` | Rename selected session (`app.session.rename`) |
| `ctrl+d` | Delete selected session (`app.session.delete`) |
| `ctrl+backspace` | Delete without exiting selector (`app.session.deleteNoninvasive`) |

**Scoped model selector** (`/scoped-models`):

| Chord | Action |
|-------|--------|
| `ctrl+s` | Save scoped patterns (`app.models.save`) |
| `ctrl+a` | Enable all (`app.models.enableAll`) |
| `ctrl+x` | Clear all (`app.models.clearAll`) |
| `ctrl+p` | Toggle provider (`app.models.toggleProvider`) |
| `alt+up` | Move selected up (`app.models.reorderUp`) |
| `alt+down` | Move selected down (`app.models.reorderDown`) |

**Session tree / fork panel** (`/tree`, `/fork`):

| Chord | Action |
|-------|--------|
| `left` | Fold the current node, or move up (`app.tree.foldOrUp`) |
| `right` | Unfold the current node, or move down (`app.tree.unfoldOrDown`) |
| `alt+shift+l` | Edit node label (`app.tree.editLabel`) |
| `alt+shift+t` | Toggle label timestamps (`app.tree.toggleLabelTimestamp`) |
| `ctrl+d` | Default filter (`app.tree.filter.default`) |
| `ctrl+t` | No-tools filter (`app.tree.filter.noTools`) |
| `ctrl+u` | User-only filter (`app.tree.filter.userOnly`) |
| `ctrl+l` | Labeled-only filter (`app.tree.filter.labeledOnly`) |
| `ctrl+a` | All entries filter (`app.tree.filter.all`) |
| `ctrl+o` | Cycle filter forward (`app.tree.filter.cycleForward`) |
| `ctrl+shift+o` | Cycle filter backward (`app.tree.filter.cycleBackward`) |

Up/Down/PageUp/PageDown move the selection in all selector and tree panels.

## Custom keybindings

Place keybinding files in one of the keybinding paths:

- Global: `<pi-dir>/keybindings.json` (the `.pi` directory under the user's
  home directory)
- Project: `<workspace>/.pi/keybindings.json`

Project files overlay global files, which overlay built-in defaults (`KeyBindingsManager::load` in `keybindings.rs:267`). A file with any malformed chord, unknown action, or duplicate canonical chord is rejected in full with a diagnostic.

Two file formats are accepted:

**Upstream action-to-chord object** (preferred):

```json
{
  "tui.input.submit": ["ctrl+enter"],
  "tui.input.newLine": ["shift+enter", "ctrl+j"],
  "app.exit": "ctrl+q"
}
```

**Legacy pi-rs `bindings` array**:

```json
{
  "bindings": [
    { "chord": "ctrl+enter", "action": "editor_submit" },
    { "chord": "enter", "action": "editor_newline" },
    { "chord": "ctrl+q", "action": "quit" }
  ]
}
```

Overriding an action replaces all of its earlier chords, matching upstream
behavior.

Valid actions are the stable IDs listed above. The canonical registry is
`VALID_ACTION_NAMES` in `keybindings.rs:177`. Chords use `+`-separated
modifiers and a key token. Modifiers are `ctrl` (alias `control`), `alt`
(alias `option`), and `shift`. Key tokens include special names (`enter`,
`tab`, `esc`, `left`, `right`, `up`, `down`, `home`, `end`, `backspace`,
`delete`, `space`, `pageup`, `pagedown`, `f1`–`f12`) or a single Unicode
scalar (`a`, `/`, `1`).

## Selectors, tree, fork, and dialogs

The TUI uses several modal page overlays inside the inline viewport. Typing
filters the list, `Enter` confirms the selection, and `Esc` closes the overlay.

- **Model selector** (`ctrl+l` or `/model` with no argument): searchable list
  of configured providers and models (`open_model_panel` in `tui.rs:1963`).
- **Thinking level selector** (`/settings` → "Thinking level" or `/theme` not
  used): one entry per level (`open_thinking_panel` in `tui.rs:1984`).
- **Settings panel** (`/settings`): toggles thinking level, theme, and
  automatic compaction (`open_settings_panel` in `tui.rs:2027`).
- **Trust panel** (`/trust`): choose `Trusted`, `Untrusted`, or `Ask` for the
  current project (`open_trust_panel` in `tui.rs:2080`).
**Saved session selector** (`/sessions`, `/resume` with no argument):
unified catalog scoped to the current cwd, filter by name/path, sort by newest
or name, rename (`ctrl+r`), and delete (`ctrl+d`) saved sessions. Rows show a
`[source]` badge; foreign source files cannot be renamed or deleted, while
native Pi sessions and already-imported conversions can be
(`handle_session_selector_key` in `tui.rs:2281`).
- **Scoped model selector** (`/scoped-models`): enable/disable models for
  `ctrl+p`/`ctrl+shift+p` cycling, save the scope, and reorder enabled models
  (`handle_scoped_model_selector_key` in `tui.rs:2387`).
- **Session tree** (`/tree`): browse the branching message tree, fold/unfold,
  apply filters, and edit node labels (`handle_tree_panel_key` in
  `tui.rs:2143`).
- **Fork panel** (`/fork`): a tree filtered to user messages; selecting one
  and pressing `Enter` copies the active path up to that message into a new
  session (`TreePanelMode::Fork` in `tree_panel.rs:9`, `open_fork_panel` in
  `tui.rs:2066`).

**Extension dialogs** are raised when an extension requests interactive UI
(`ExtensionDialog` in `tui.rs:127`). They include:

- **Select**: `Up`/`Down` to choose, `Enter` to accept, `Esc` to cancel.
- **Confirm**: arrow keys or `Tab` to swap the default; `y`/`Y` or `n`/`N` or
  `Enter` to accept; `Esc` to cancel.
- **Input / Editor**: full editor keys from the keybinding table, `Enter`
  (`tui.input.submit`) to accept, `Esc` to cancel.

(`handle_extension_dialog_key` in `tui.rs:2576`)

## Terminal image behavior

Images are rendered only in the inline TUI. The line REPL and structured modes
never receive image protocol bytes (`terminal_images.rs:3`).

**Protocol detection** is deterministic from environment variables
(`detect_protocol` in `terminal_images.rs:62`):

- **Kitty**: `KITTY_WINDOW_ID`, `TERM=xterm-kitty`, or `TERM_PROGRAM` equal to
  `wezterm`/`ghostty`.
- **iTerm2**: `TERM_PROGRAM=iTerm.app` **and** `ITERM_SESSION_ID` present.
- **Sixel**: `TERM` contains `sixel`. Sixel is detected for truthful
  diagnostics but is **not emitted** because the project does not yet include
  a bounded safe encoder; the TUI falls back to metadata text
  (`supports_images` in `terminal_images.rs:283`).

Multiplexers (`TMUX`, `STY`) deliberately report no protocol because their
passthrough configuration cannot be proven from the child environment.

**Image display settings** come from `terminal.showImages` and
`terminal.imageWidthCells` in `settings.json` (`TerminalSettings` in
`pi-coding/src/settings.rs:64`, default width is `60` from `tui_runtime` in
`settings.rs:598`). Images are suppressed whenever any overlay (selector,
tree, dialog) is open (`tui.rs:3618`).

**Layout and safety**: the renderer decodes and validates the image, refuses
empty data and payloads larger than the configured limit (`MAX_IMAGE_BYTES`
in `image_pipeline.rs:10`), clamps the layout to the viewport and
`imageWidthCells`, preserves aspect ratio, and falls back to ordinary metadata
when display is disabled or the protocol is unsupported (`TerminalImageRenderer::layout` in
`terminal_images.rs:294`).

**Presentation**: only the current frame is cached; unchanged frames are not
retransmitted. Kitty images are emitted with chunked payloads
(`KITTY_CHUNK_BYTES = 4_096` in `terminal_images.rs:20`) and deleted by ID
on cleanup; iTerm images use the inline `1337` sequence with
`preserveAspectRatio=1` (`write_iterm_image` in `terminal_images.rs:607`).

## Slash commands

The TUI and line REPL can execute the same built-ins, but `/help`, slash
completion, and RPC discovery expose only the 14 commands in
`PRIMARY_COMMAND_NAMES` (`interactive_commands.rs`). Prompt templates, dynamic
skills, and extension commands remain executable through their namespaced paths.

| Command | Description |
|---------|-------------|
| `/settings` | Inspect settings or open the settings page |
| `/model [provider/model]` | Select or switch model |
| `/branch` | Create a branch from a previous message |
| `/resume [path, id, or prefix]` | Resume native or discovered foreign sessions |
| `/fork` | Fork from a previous user message |
| `/export [path]` | Export session to HTML or JSONL |
| `/agents` | Manage agent definitions and model overrides |
| `/compact [instructions]` | Manually compact session context |
| `/ps` | List supervised processes |
| `/loop [interval] <prompt>` | Run a recurring prompt |
| `/goal` | Manage the durable session goal |
| `/workflow` | Manage isolated concurrent workflows |
| `/code-review [<from> <to>]` | Open a fullscreen Git diff browser: bare shows tracked HEAD→working-tree changes; two refs compare any two commits/branches/tags |
| `/btw [prompt]` | Open a persistent detached side conversation forked from the active main branch |

Other built-ins such as `/help`, `/new`, `/sessions`, `/tree`, `/todo`,
`/share`, `/copy`, `/login`, `/logout`, `/process`, `/theme`, and `/quit`
remain manually executable but are intentionally omitted from primary discovery.

`/btw` is read-only by default. `Ctrl+T` toggles edit/exec tools while the side agent is idle; `Esc` aborts a streaming turn or closes an idle overlay; `Alt+R` reforks from the current main leaf; `Alt+N` clears the side transcript. The overlay has its own editor, transcript, events, stream, and abort lifecycle. Closing it preserves the side controller for reopening, while TUI shutdown aborts and joins active side work.

`/code-review` displays staged and unstaged tracked changes against `HEAD`. `Tab` changes pane focus; `j`/`k`, arrows, and mouse clicks select hunks; `c` comments on the selected hunk; `Space` folds its inline review thread. Each thread appears after the complete hunk body with distinct comment/answer cards, and repeated agent progress messages collapse into one answer per exchange. Page keys and the mouse wheel scroll; tree clicks select or fold; `Esc`/`q` closes the page. Mouse capture is enabled only for this page and restored on every close or overlay transition.

`/code-review` accepts zero or exactly two arguments. Bare `/code-review` shows
tracked HEAD→working-tree changes. `/code-review <from> <to>` resolves each ref
(commit hash, branch, or tag) to a commit and renders the commit-to-commit diff
labeled `<from> → <to>` in the panel title; working-tree changes are ignored. One
argument or more than two is invalid — the page does not open and the status line
shows `Usage: /code-review [<from> <to>]`. Pressing `r` refreshes the snapshot and
preserves the selected revision pair.

Loop intervals accept positive bare seconds (`/loop 300 check status`) or compact `s`, `m`, `h`, and `d` units (`/loop 3s echo hello`, `/loop 30m check deploy`). Scheduled turns appear as `Loop <id> · <cadence>` system cards; the internal model instruction wrapper is not shown as a user message.

## TUI vs REPL-only limitations

When stdout is not a TTY, `main_run` falls back to `repl::interactive`
(`lib.rs:142`). The REPL shares the same slash-command
catalog but lacks the TUI's modal page overlays:

- **No modal pages**: model, settings, trust, saved-session, scoped-model,
  session-tree, fork, `/btw`, and `/code-review` overlays are TUI-only. In the
  REPL, `/scoped-models` and `/theme` report that they require the TUI, `/tree`
  prints JSON, `/fork` with no argument prints candidate messages, and `/model`
  accepts a concrete spec.
- **No configurable keybindings or themes**: the REPL uses a fixed
  line-editing interface and ignores theme files.
- **No terminal image rendering**: image attachments are processed and sent
  to the model, but the REPL displays only text metadata.
- **No interactive extension UI**: the REPL is composed without the TUI's
  `ExtensionUiAdapter` (`session_run.rs:491`), so extension select/confirm/input
  dialogs cannot be answered interactively.
