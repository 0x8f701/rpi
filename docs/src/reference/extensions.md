# Extensions and process protocol

Extensions contribute capabilities to the host through either a process runtime or the in-process QuickJS runtime. They are admitted resources, not arbitrary project code that loads before trust.

## Load and validate

`--extension PATH` explicitly selects an extension manifest or directory and is repeatable. `--no-extensions` disables configured/discovered extensions while retaining explicit paths.

A manifest identifies its schema version, identifier, optional package version, runtime, capabilities, and UI capabilities. Process runtime configuration supplies an executable and arguments; it must not also set a QuickJS entry. QuickJS requires an entry and supports `.js` or `.mjs`, not TypeScript source.

Manifest validation checks identity, version, runtime path, and capability coherence before runtime creation. Runtime paths are resolved through the resource capability boundary.

## Capabilities

The capability vocabulary includes `commands`, `tools`, `event_hooks`, `message_renderers`, `provider_metadata`, `provider`, `session_actions`, `ui`, and `overlays`. Declaring UI operations without the `ui` capability is invalid. Runtime-specific validation can further restrict supported combinations.

UI capabilities include selection, confirmation, input/editor requests, notification, status, widgets, title, editor text, working state, hidden thinking, theme, tool expansion, and overlays. The host owns rendering and focus; an overlay supplies content rather than replacing the terminal frame.

## Process boundary

The extension protocol version is **1**. Process communication is framed through the host's request/response runtime with bounded messages. The default maximum frame size is **1 MiB**; manifests also have a **1 MiB** bound. Request timeouts are capped at **10 minutes**, and diagnostic stderr retains a **16 KiB** tail.

Extension process transport is not the public JSONL session control protocol and is not Agent Client Protocol. A process extension must implement the extension handshake and declared capabilities; merely printing JSON to standard output is not a valid registration.

## Packages and plugins

Resource packages can supply extensions. Marketplace `rpi plugin` commands manage packaged extensions separately from the local/Git package backend. Installation does not itself grant runtime trust. See [packages](packages.md).

## Security

An extension can introduce executable behavior and provider or session actions. Review its code, manifest, and requested capabilities before approval. Tool allow/deny filtering applies after contributions are assembled; `--no-tools` removes all tool contributions, including extensions. A failed candidate reload must not partially replace the active runtime generation.
