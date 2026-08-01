# Local / self-hosted models with llama.cpp

`pi` has first-class support for a local [llama.cpp](https://github.com/ggerganov/llama.cpp)
router. When a router is configured, its live models are merged into the
catalog under the `llama.cpp` provider and can be selected like any other
model.

## Configure a router

```sh
pi llama configure http://localhost:8080 [--api-key TOKEN]
```

Source: `crates/pi-cli/src/args.rs:307-314`,
`crates/pi-cli/src/llama_commands.rs:14-27`.

`pi llama configure` validates the router by calling its `/v1/models` endpoint
before persisting the settings. The base URL is normalized: a trailing `/v1` is
stripped, query strings and fragments are removed, embedded credentials are
rejected, and only `http`/`https` schemes are allowed. The configuration is
persisted in the agent directory under the llama data directory.
Source: `crates/pi-ai/src/llama.rs:38-50`,
`crates/pi-ai/src/llama.rs:359-377`,
`crates/pi-coding/src/llama.rs:173-194`.

You can also set `LLAMA_BASE_URL` and optionally `LLAMA_API_KEY` to skip
explicit configuration. On Unix, the persisted settings file must not be
readable by group or other users.
Source: `crates/pi-coding/src/llama.rs:130-159`.

## Use local models

Once configured, local models appear as `llama.cpp/<MODEL_ID>`:

```sh
pi -m llama.cpp/<model-id> --print "Hello"
```

Router models are refreshed automatically at startup unless `PI_OFFLINE` is
set. If the router is unreachable, `pi` falls back to the cached catalog with a
warning and continues using the last successfully observed snapshot.
Source: `crates/pi-cli/src/session_run.rs:284-299`.

## Manage the router

| Subcommand | Purpose |
|------------|---------|
| `pi llama status` | Show configured router and live models |
| `pi llama status --reload` | Ask the router to rescan its model directory |
| `pi llama refresh` | Refresh live models; fall back to cache |
| `pi llama load MODEL` | Load a model through the router |
| `pi llama unload MODEL` | Unload a model through the router |

Source: `crates/pi-cli/src/args.rs:316-332`,
`crates/pi-cli/src/llama_commands.rs:24-69`.

`status` prints each router model on its own line with a status flag such as
`loaded`, `loading`, `unloaded`, or `sleeping`.
Source: `crates/pi-cli/src/llama_commands.rs:24-37`.

`load` and `unload` request the router to change its active model, then refresh
the live catalog and persist the new snapshot atomically.
Source: `crates/pi-coding/src/llama.rs:247-290`.

In the TUI or REPL, `/llama` accepts the same operations:

```text
/llama status
/llama refresh
/llama load llama-3.1-8b
/llama unload llama-3.1-8b
/llama configure http://localhost:8080 [TOKEN]
```

Source: `crates/pi-cli/src/interactive_commands.rs:260-264`,
`crates/pi-cli/src/llama_commands.rs:156-222`.

## Download GGUF models from Hugging Face

```sh
# Search
pi llama search "meta-llama/Llama-3.1-8B"

# List quantizations and file checksums
pi llama details meta-llama/Llama-3.1-8B-GGUF

# Download a quantization (or the first available one if -q is omitted)
pi llama download meta-llama/Llama-3.1-8B-GGUF -q Q4_K_M

# List local downloads
pi llama installed
```

Source: `crates/pi-cli/src/args.rs:333-353`,
`crates/pi-cli/src/llama_commands.rs:66-107`.

`search` returns repository ids and download counts. `details` lists each
quantization, the files it contains, their sizes, and SHA-256 checksums when
available. `download` installs the selected quantization into the agent's llama
models directory.
Source: `crates/pi-ai/src/llama.rs:494-525`,
`crates/pi-ai/src/llama.rs:526-633`,
`crates/pi-coding/src/llama.rs:294-413`.

Downloads are:

- Atomic: written to a `.part` file and renamed into place only after the
  checksum succeeds.
- Resumable: a partial `.part` file is reused with an HTTP `Range` request.
- Verifiable: each file is checked against the SHA-256 from Hugging Face when
  a checksum is provided.
- Cancellable: pressing Ctrl-C cancels the in-progress download cleanly.

Source: `crates/pi-coding/src/llama.rs:439-638`.

Authentication uses the `HF_TOKEN` environment variable or an already-configured
Hugging Face token. A custom Hugging Face endpoint can be set with `HF_ENDPOINT`.

## Authentication

If your router requires a bearer token, pass it with `--api-key` during
configuration or set `LLAMA_API_KEY`. The key is sent as an
`Authorization: Bearer` header on router management and inference requests.
Source: `crates/pi-coding/src/llama.rs:130-159`,
`crates/pi-cli/src/models_config.rs:295-308`.

## Environment variables

| Variable | Purpose |
|----------|---------|
| `LLAMA_BASE_URL` | Router base URL (skips `pi llama configure`) |
| `LLAMA_API_KEY` | Router bearer token |
| `HF_TOKEN` | Hugging Face token for GGUF search/download |
| `HF_ENDPOINT` | Custom Hugging Face API endpoint |
| `PI_OFFLINE` | Skip router refresh at startup |
| `PI_CODING_AGENT_DIR` | Override the agent directory that stores llama settings and downloads |
