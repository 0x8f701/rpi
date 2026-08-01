# Custom theme and keybindings example

TUI themes and keybindings are implemented in this release.

## Custom theme

Create `$PI_CODING_AGENT_DIR/themes/solarized.json` or
`$CWD/.pi/themes/solarized.json`:

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
    "toolDiffAdded": "green",
    "toolDiffRemoved": "#dc322f",
    "toolPendingBg": "#073642"
  }
}
```

Activate it with `/theme solarized` in the TUI or by setting `"theme": "solarized"`
in `settings.json`.

## Custom keybindings

Create `$PI_CODING_AGENT_DIR/keybindings.json` or `$CWD/.pi/keybindings.json`:

```json
{
  "bindings": [
    { "chord": "ctrl+enter", "action": "editor_submit" },
    { "chord": "enter", "action": "editor_newline" },
    { "chord": "ctrl+q", "action": "quit" },
    { "chord": "f5", "action": "theme_next" }
  ]
}
```

Project keybindings overlay global keybindings, which overlay built-in defaults.
A malformed file is rejected in full.

See [`docs/tui.md`](../docs/tui.md) for the full list of color roles, actions,
and chord syntax.
