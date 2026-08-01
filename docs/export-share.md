# Export and share

`rpi` can export a session to a self-contained HTML file or to a Pi v3 JSONL
branch, and it can share a session as a private GitHub gist. The export step
needs no model, auth, or network access; only gist sharing requires the `gh`
CLI.

## Export a session from the CLI

```sh
# Export a session file to a self-contained HTML file
rpi export <agent-dir>/sessions/--cwd--/timestamp_id.jsonl

# Export to a specific path
rpi export session.jsonl --output report.html

# Export the current branch as JSONL (suitable for --resume)
rpi export session.jsonl --jsonl

# JSONL output with an explicit path
rpi export session.jsonl --jsonl --output backup.jsonl
```

Source: `crates/pi-cli/src/args.rs:232-247`,
`crates/pi-cli/src/commands.rs:129-145`.

`--output` sets the destination path. Without it, the output path is derived
from the session file by swapping the extension to `.html` or `.jsonl`. Writes
are atomic: a temporary file is created in the same directory and renamed into
place.
Source: `crates/pi-coding/src/export/mod.rs:91-145`,
`crates/pi-coding/src/export/mod.rs:693-732`.

### HTML export

HTML export produces a single self-contained `.html` file with inline CSS and
JavaScript and no external dependencies. The rendered transcript is the full
chronological record from the session file, including compaction markers. All
user, model, and tool content is HTML-escaped at render time, so arbitrary
markup cannot escape into the page.
Source: `crates/pi-coding/src/export/mod.rs:1-10`,
`crates/pi-coding/src/export/mod.rs:188-204`.

### JSONL export

JSONL export writes only the current branch (root → leaf) of the session and
produces a valid Pi v3 session file. It is suitable for archiving or for passing
to `--resume`.
Source: `crates/pi-coding/src/export/mod.rs:107-132`.

## Export during a session

In the TUI or line REPL, use the `/export` slash command:

```text
/export                  # write HTML to the default path
/export report.html      # write HTML to the named file
/export backup.jsonl     # write the current branch as JSONL
```

If the argument ends in `.jsonl` (case-insensitive), the export is JSONL;
otherwise it is HTML. The path is printed to the REPL or shown as a TUI status
message.
Source: `crates/pi-cli/src/repl.rs:321-332`,
`crates/pi-cli/src/tui.rs:3255-3261`,
`crates/pi-cli/src/interactive_commands.rs:160-164`.

### RPC export

In RPC mode, send:

```json
{"type":"export_html","outputPath":"report.html"}
```

The response contains `{ "path": "..." }`. RPC export always produces HTML.
Source: `crates/pi-cli/src/modes/rpc.rs:138-141`,
`crates/pi-cli/src/modes/rpc.rs:894-896`.

## Share a session as a gist

```text
/share
```

`/share` is available in the TUI, the REPL, and through the application API. It
exports the current session to HTML and uploads it as a **private** GitHub gist
using `gh gist create --private --desc "Pi session export"`.
Source: `crates/pi-cli/src/interactive_commands.rs:170-174`,
`crates/pi-cli/src/repl.rs:333-349`,
`crates/pi-coding/src/share.rs:147-163`.

Requirements:

- `gh` must be installed.
- `gh auth login` must have completed successfully.

If either check fails, the command returns an actionable error such as
"`gh CLI not found; install it from https://cli.github.com/`" or
"`gh is not authenticated; run `gh auth login` to sign in`".
Source: `crates/pi-coding/src/share.rs:40-60`.

By default the returned URL is the raw gist URL, which renders on
`gist.github.com`. You can substitute a custom viewer by setting
`PI_SHARE_VIEWER_URL`. If the value contains `{url}`, the gist URL is substituted
into the template; otherwise the value is used verbatim.
Source: `crates/pi-coding/src/share.rs:20-24`,
`crates/pi-coding/src/share.rs:67-89`.

## Copy to clipboard

The last assistant response can be copied to the system clipboard:

- Slash command: `/copy`
- Default keybindings: `ctrl+x` or `ctrl+shift+c` (action name `app.message.copy`
  / `copy_last_assistant`)

Source: `crates/pi-cli/src/repl.rs:350`,
`crates/pi-cli/src/interactive_commands.rs:175-179`,
`crates/pi-cli/src/keybindings.rs:53-59`,
`crates/pi-cli/src/keybindings.rs:128-135`,
`crates/pi-cli/src/keybindings.rs:396-401`.

Clipboard writing is implemented per platform: PowerShell on Windows,
`pbcopy` on macOS, and `xclip`/`xsel` on Linux. The TUI and REPL also support
pasting clipboard images with `ctrl+v` / `alt+v` (action `app.clipboard.pasteImage`
/ `clipboard_paste`).
Source: `crates/pi-cli/src/clipboard.rs:52-112`.

## Manual portability

Native Pi v3 session files are plain JSONL and live under:

```text
<agent-dir>/sessions/--<encoded-cwd>--/<timestamp>_<id>.jsonl
```

You can copy, move, or archive these files directly. A session file may contain
multiple branches linked by `parentId`; the active branch is followed from the
leaf entry. The JSONL export writes only that active branch.

## Environment variables

| Variable | Purpose |
|----------|---------|
| `PI_SHARE_VIEWER_URL` | Viewer URL template; use `{url}` to substitute the gist URL |
