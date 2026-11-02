# Quick start

## Configure a provider

After [installation](install.md), inspect setup and configure a model provider:

```sh
rpi setup
rpi login
rpi models
```

Login can ask you to select a provider. Model listing reflects configured credentials, not just every identifier known to the application. Keep keys out of shell history and shared files.

## Start in a project

From an existing project directory:

```sh
rpi
```

Use the model selector to choose an available provider/model and enter a concrete request. Ctrl+L opens the terminal model selector. Enter submits; Shift+Enter or Ctrl+J inserts a newline; Esc is the keyboard Stop action.

`rpi -C DIR` selects an existing working directory. CLI startup does not create a missing directory. Review project trust before admitting project-local settings, extensions, skills, or prompt templates.

## Noninteractive work

```sh
rpi --print "Explain the project structure without editing files"
rpi --mode json "List the main modules"
```

Use `--model PROVIDER/MODEL` to select an explicit available model. Noninteractive prompts use the same tools and authority boundaries as interactive work; asking for read-only work is not a substitute for a confined read-only policy.

## Continue a conversation

```sh
rpi --continue
rpi sessions
```

`--continue` selects the most recent session for this directory. `--resume PATH_OR_ID` selects a specific native or supported foreign session. [Session recovery](../user-guide/session-recovery.md) explains import, rewind, checkpoints, and export.

## Browser interface

```sh
rpi --listen 127.0.0.1:8543
```

Open the printed HTTPS address. The listener runs without the terminal interface and remains alive independently of standard input. Use the pairing URL or token card when authentication is required; protect the pairing URL as a credential.

The Web client includes separate Loop and Processes panels. Loop schedules recurring prompts; Processes runs supervised commands in the session working directory. Files offers Source/Preview views with safe Markdown and sandboxed HTML previews.

## Next steps

- [CLI modes](../user-guide/cli-modes.md): arguments and command families.
- [TUI](../user-guide/tui.md): bindings and panels.
- [Web](../user-guide/web.md): pairing, files, loops, and processes.
- [Settings and trust](../reference/settings-trust.md): configuration and authority.
- [Security](../reference/security.md): confinement and rendering boundaries.

Steering queue cancellation is still being integrated on the Rust side; Web queue previews do not imply a stable scoped-cancellation or recovery contract.
