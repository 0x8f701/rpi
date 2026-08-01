# pi examples

The compiled examples live in `examples/src/bin/` and can be run from the
`examples/` directory with:

```sh
cargo run --bin json_events
cargo run --bin rpc_client
cargo run --bin prompt_template
cargo run --bin custom_tool
cargo run --bin process_extension
```

The first four examples use the [`faux`](../crates/pi-ai/src/providers/faux.rs)
provider, so they need no real API key and produce deterministic output. The
`process_extension` example launches its own local child process.

## Available examples

| Example | What it shows |
|---------|---------------|
| `json_events` | Subscribing to `pi_coding::ApplicationEvent` and serializing each event to a JSON line. The same event stream is used by the CLI's `--mode json`. |
| `rpc_client` | Wrapping the same application events as JSON-RPC 2.0 notifications. The CLI also ships a real `--mode rpc` stdio server; see [`docs/rpc-json.md`](../docs/rpc-json.md). |
| `prompt_template` | Building a system prompt from a custom template, project context files, and skills. |
| `custom_tool` | Registering a custom `AgentTool` that shells out to an external process. For a full process extension, see `process_extension`. |
| `process_extension` | Building an in-process extension runtime, launching a child extension process, and exercising tools/commands/UI/event hooks through the `pi-extension.json` protocol. |

## Additional examples

- [`theme_keybindings.md`](theme_keybindings.md) — custom TUI theme and keybinding JSON files.
- [`process_extension.md`](process_extension.md) — a minimal `pi-extension.json` process extension.
- [`bun_extension.ts`](bun_extension.ts) — an OMP-style default-export factory
  hosted by optional Bun through a `runtime: "bun"` manifest.

> **Note:** The `examples/` crate is not part of the workspace. It declares its
> own `[workspace]` and uses path dependencies to the sibling crates so it can
> be built independently.
