# TUI and keybindings

Run `rpi` in a terminal for the interactive interface. The composer, transcript, status, selectors, and panels share the active session. Use [CLI modes](cli-modes.md) for noninteractive output and [Web](web.md) for the browser interface.

## Composer and transcript

| Default key | Action |
| --- | --- |
| Enter | Submit |
| Shift+Enter or Ctrl+J | Insert newline |
| Tab | Accept completion |
| Up / Down | Move in the editor or prompt history |
| Alt+Left / Alt+Right | Move by word |
| Home / End | Line start / end |
| Ctrl+W | Delete previous word |
| Ctrl+U | Clear composer |
| Ctrl+K | Delete to line end |
| Ctrl+Y / Alt+Y | Yank / cycle yank |
| Ctrl+- | Undo |
| PageUp / PageDown | Page transcript |
| Esc | Stop active work or dismiss the current interaction |
| Ctrl+C | Clear input; repeated clear/quit handling is contextual |
| Ctrl+D | Quit outside selectors that bind it to deletion |
| Ctrl+G | External editor |
| Ctrl+Z | Suspend |

Key handling is contextual: a selector owns its keys while focused. Do not infer that a chord has the same meaning in every panel.

## Models, output, and files

| Default key | Action |
| --- | --- |
| Ctrl+L | Model selector |
| Ctrl+P / Ctrl+Shift+P | Cycle scoped models forward / backward |
| Shift+Tab | Cycle thinking level |
| Ctrl+T | Toggle thinking visibility |
| Ctrl+O | Expand tool output |
| Alt+V | Paste an image; Ctrl+V is also bound but terminal support varies |
| Ctrl+X or Ctrl+Shift+C | Copy last assistant output |
| Alt+O | Read-only file viewer |
| Alt+/ | Switch focus between a supported overlay and composer |
| Ctrl+Space | Push-to-talk when `/live` is enabled |

The file viewer can use workspace path candidates from the transcript or reopen its previous state. Browser-only HTML and SVG previews are described in the Web guide; they are not terminal HTML execution.

## Sessions and panels

The session selector supports named-session filtering, path display, sorting, rename, and deletion. Its default keys include Ctrl+N, Ctrl+P, Ctrl+S, Ctrl+R, and Ctrl+D respectively. Session tree filters and model-scope selectors have their own bindings.

Use `/hotkeys` to inspect active bindings and `/help` to inspect interactive commands. Goals, Todos, workflows, subagents/jobs, supervised processes, and recurring loops are distinct features rather than interchangeable task lists.

The shared interrupt label is **Stop**, with Esc as its keyboard synonym. Wire commands can still use the identifier `abort`.

**Steering queue status: in progress.** The keybinding registry includes follow-up and dequeue actions, and Web queued-prompt previews are implemented. Rust steering-queue cancellation is still being integrated, so its scope/index behavior is not a stable documented contract yet.

## Configure bindings

Bindings are loaded over built-in defaults in global-first, project-last order. Project bindings override global bindings when the project resource layer is accepted. Stable action identifiers include `tui.input.submit`, `tui.input.newLine`, `app.interrupt`, `app.model.select`, and `app.file.viewerOpen`.

Validation is atomic per binding file: malformed chords, unknown actions, or duplicate normalized chords reject the entire file. Previously active bindings remain in place rather than accepting half a file. See [settings and trust](../reference/settings-trust.md) for project resource authority.

## Safety

Transcript and panel content are bounded for terminal rendering. Long tool output is not proof that the complete output is visible; follow tool truncation information or open the relevant artifact. Review approvals before granting host execution or write authority.
