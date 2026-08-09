# Extended tool catalog

Beyond the core coding tools (`read`, `bash`, `edit`, `write`, `grep`,
`find`, `glob`, `ls`), `rpi` ships a catalog of extended tools
(`TOOL_NAMES` in `crates/pi-coding/src/tools.rs:153`). Each tool below is a
built-in; some are session-scoped (one live child process per session), and
all bound their output and redact secrets.

## `lsp`

Query a language server over the Language Server Protocol (JSON-RPC 2.0 with
Content-Length framing, `lsp-types` message shapes). Actions: hover,
definition, references, diagnostics, symbols, rename, code actions; one
server is spawned per invocation (`crates/pi-coding/src/tools/lsp_client.rs`).
`initialize` runs against the workspace root; a dedicated
`wait_for_diagnostics` waits for the push targeted at a specific document
URI. `ContentModified` errors are retried. Server stderr is captured
(bounded) and redacted in error messages. Timeouts: 30 s per request, 15 s
diagnostics wait.

## `browser`

Headless Chromium/Chrome automation over the Chrome DevTools Protocol
(`crates/pi-coding/src/tools/browser.rs`). Actions: `navigate`, `click`,
`fill`, `screenshot`, `extract`, `list_tabs`, `close`. Each call spawns a
fresh headless browser with a temporary profile, performs exactly one action,
then tears it down — state does not persist between calls; non-navigate
actions accept an optional `url` to navigate first. Chrome is discovered via
`CHROME_PATH`, PATH, or standard install locations; missing binaries are
rejected actionably. Output is bounded (16 KiB extract cap). Timeout/abort
kills the whole browser process group, so a compromised page can only act
within one action's window.

## `github`

GitHub API access via the `gh` CLI (preferred — uses the user's own gh auth;
rpi never touches or prints a token) with a `GH_TOKEN`/reqwest fallback
(`crates/pi-coding/src/tools/github.rs`). Actions: `search_issues`,
`get_issue`, `list_issues`, `create_issue`, `comment_issue`, `list_prs`,
`get_pr`, `list_commits`, `view_file`, `search_code`. Requests are
argv-built (`gh api --method … -f key=value`, no shell interpolation);
GitHub JSON is parsed into bounded plain text (32 KiB cap; code search
renders `file:line:snippet`). All surfaced error text is redacted.

## `ask`

Ask the user a question mid-task and receive the typed answer as the tool
result (`crates/pi-coding/src/ask.rs`). One pending question at a time;
publishes `SessionEvent::AskUser` so the frontend renders the prompt, and
resolves the awaiting call when the answer arrives. **Interactive sessions
only**: print/JSON/RPC/REPL never arm the interactive flag, so the tool
rejects up front instead of hanging. Answer-wait bound: 60 s default; abort
or cancel resolves the slot.

## `eval`

Evaluate code in a persistent, session-scoped language kernel
(`crates/pi-coding/src/tools/eval.rs`): `python` (a real `python3`
subprocess with the full standard library — an execution tool, not a
sandbox) or `js` (the embedded QuickJS engine with no `require`/`import`, no
network, and no filesystem access). Globals persist between calls
(cross-cell state); a cell timeout (default 30 s, max 300 s) kills the
kernel and the next call respawns it. Output is bounded (64 KiB per stream),
errors are classified (syntax/runtime/timeout), and results are redacted.

## `notebook`

Read, execute, and edit Jupyter notebooks (`.ipynb`)
(`crates/pi-coding/src/tools/notebook.rs`): `read` lists cells (8 MiB file
cap, 200-cell preview), `execute` runs code cells through the same
session-scoped Python kernel as `eval` (outputs written back only with
`write=true`; unknown fields preserved), `edit` appends a markdown/code/raw
cell. Per-action capability gating: `read`→Read, `execute`→Exec,
`edit`→Write, enforced before any dispatch — a read-only role can read
notebooks without gaining edit/execute.

## `debug`

Session-scoped Debug Adapter Protocol (DAP) client over stdio
(`crates/pi-coding/src/tools/debug.rs`). Adapters: `gdb`, `lldb-dap`,
`debugpy`. Actions: `launch`, `set_breakpoint` (1-based `file:line`),
`continue_`, `pause`, `step_over`/`step_in`/`step_out`, `stack_trace`,
`variables`, `evaluate`, `threads`, `terminate`. The program stays paused
before start until the first `continue_` sends `configurationDone`; one
adapter per session; `terminate` (or drop) kills the whole process group,
debuggee included. Bounded: 30 s request timeout, 50 frames, 200 variables,
32 KiB rendered output; adapter stderr is redacted in launch failures.

## `generate_image` and `inspect_image`

- `generate_image` — bounded image generation through the provider subsystem
  (`pi_ai::generate_image`, OpenAI-compatible `images/generations`). The
  model must declare image capability (active model, an explicit `model`
  argument, or `settings.images.genModel`); `images.genBaseUrl`/`genApiKey`
  override the endpoint/credential for self-hosted services. Prompts capped
  at 4096 characters, `n` at 4, sizes whitelisted to 256/512/1024 square,
  decoded output capped at 128 MiB per image with a 16 MP header pre-check,
  save path workspace-contained. Returns file paths plus a bounded prompt
  echo — image bytes never enter the transcript.
- `inspect_image` — deterministic metadata + statistics for an image file
  without rendering it: format, dimensions, decoded color type, file size,
  EXIF orientation (JPEG/WebP), mean/stddev 8-bit luma brightness, and a
  coarse 8-bin RGB dominant-color histogram. Refuses files over 32 MiB from
  metadata before reading; decoded allocation capped at 128 MiB; output
  bounded to a few KiB. No OCR, no ML, no vision-model call.

## `web_search`, `ast_grep`, `ast_edit`

- `web_search` — DuckDuckGo Instant Answer API search (disabled while
  `PI_OFFLINE` is set).
- `ast_grep` — structural code search with ast-grep patterns
  (`$-metavariables`, tree-sitter).
- `ast_edit` — single-file structural rewrite with ast-grep
  pattern→rewrite.

## Memory tools

`memory` (local JSONL store) and `recall`/`retain`/`reflect` (Hindsight
backend) are documented in [`memory.md`](memory.md). `mcp`, `debug`, `eval`,
`notebook`, `lsp`, `browser`, and `github` above plus the orchestration
tools (`task`, `hub`, `yield`, `goal` — see
[`orchestration.md`](../user-guide/orchestration.md)) and the `todo` tool
(see [`todos.md`](../user-guide/todos.md)) make up the full built-in set.

## Tool schemas and example calls

Every tool validates its arguments against a JSON-schema-style `parameters`
object before execution; unknown keys and invalid values fail with actionable
messages. Schemas below are verbatim from the tool factories; `required`
lists the parameters the schema marks as required (all others are optional).
Nullable-but-required parameters (the `todo` tool, orchestration tools) must
still be present in the call object, but may be `null` when unused.

| Tool | Required | Optional | Example call |
|------|----------|----------|--------------|
| `read` | `path` | — | `read path=src/main.rs` |
| `bash` | `command` | `timeout`, `excludeFromContext`, `cwd`, `env` | `bash command="cargo test" timeout=120` |
| `browser` | `action` | `url`, `selector`, `text`, `path` | `browser action=fill url=https://example.com selector=#name text=world` (`tools/browser.rs:96-115`) |
| `github` | `action` | `repo`, `query`, `number`, `title`, `body`, `path`, `state`, `ref` | `github action=view_file repo=octocat/Hello-World path=README` (`tools/github.rs:97-131`) |
| `lsp` | `action` | `path`, `query`/`symbol`, `line`, `character`, `end_line`, `end_character`, `new_name`, `lang` | `lsp action=definition path=src/main.rs line=10 character=4` (`tools/lsp.rs:221-275`) |
| `eval` | `language`, `code` | `timeout` | `eval language=python code="x = 1"` (`tools/eval.rs:1049-1065`) |
| `notebook` | `action`, `path` | `cell`, `write`, `cell_type`, `source`, `timeout` | `notebook action=execute path=demo.ipynb write=true` (`tools/notebook.rs:128-157`) |
| `debug` | `action` | `adapter`, `program`, `args`, `cwd`, `launch_args`, `adapter_args`, `file`, `line`, `thread`, `variables_reference`, `expression`, `frame_id`, `wait_ms`, `levels` | `debug launch adapter=gdb program=./bin` → `debug set_breakpoint file=src/main.rs line=42` → `debug continue_` (`tools/debug.rs:759-823`) |
| `mcp` | `action` | `server`, `tool`, `args` | `mcp call server=my-tools tool=weather args={"city":"London"}` (`mcp.rs:667-690`) |
| `ask` | `question` | — | `ask question="Should I proceed?"` (schema `{question: string}` in `pi_agent::create_ask_tool`) |
| `web_search` | `query` | — | `web_search query="Rust async cancellation safety"` (`tools/web_search.rs:39-50`) |
| `ast_grep` | `pattern` | `path`, `lang` | `ast_grep pattern='fn $FNAME() {}' path=src lang=rust` (`tools/ast_grep.rs:63-84`) |
| `ast_edit` | `pattern`, `rewrite`, `path` | `lang` | `ast_edit pattern='Some($A)' rewrite='Option::Some($A)' path=src/lib.rs` (`tools/ast_edit.rs:66-93`) |
| `memory` | `op` | `content`, `tags`, `query`, `limit`, `tag`, `id` | `memory learn content="…" tags=["rust"]`; `memory recall query="async" limit=5`; `memory forget id=<id>` (`memory.rs:451-468`) |
| `generate_image` | `prompt` | `model`, `size`, `n`, `path` | `generate_image prompt="a cat" size=1024 n=1 path=out.png` (`tools/image_gen.rs:81-121`) |
| `inspect_image` | `path` | — | `inspect_image path=screenshot.png` (`tools/image.rs:82-90`) |
| `todo` | `op`, `list`, `task`, `phase`, `items`, `dependsOn`, `cascade` | (all nullable) | `todo op=init list=[{phase:"Plan",items:["A","B"]}]`; `todo op=start task=task-abc`; `todo op=view` (`tools.rs:671-708`) |
| `task` | `agent`/`task`, or `tasks[]` (each item needs `task`; `name`/`agent`/`todoTaskId` nullable) | `name`, `todoTaskId`, `context` | `task agent=researcher task="Study the persistence layer" todoTaskId=task-abc` (`orchestration/tools.rs:497-513`) |
| `hub` | `op` | `to`, `message`, `replyTo`, `await`, `from`, `timeoutMs`, `peek`, `ids`, `agentId`, `lines` | `hub send to=w1 message="…"`; `hub wait from=w1 timeoutMs=30000`; `hub read_history agentId=w1 lines=50` (`orchestration/tools.rs:536-606`) |
| `yield` | `text` | — | `yield text="<full final deliverable>"` (`orchestration/tools.rs:136-140`) |
| `goal` | `op` | — | `goal op=get` / `goal op=pause` / `goal op=complete` (`application.rs:3491-3508`) |

Action enums are enforced by the schema: `browser` = `navigate, click, fill,
screenshot, extract, list_tabs, close` (`tools/browser.rs:91`); `github` =
`search_issues, get_issue, list_issues, create_issue, comment_issue, list_prs,
get_pr, list_commits, view_file, search_code` (`tools/github.rs:40-50`); `lsp`
= `hover, definition, references, diagnostics, symbols, rename, code_actions,
capabilities, status, reload` (`tools/lsp.rs:64`); `notebook` = `read,
execute, edit` (`tools/notebook.rs:62`); `debug` = `launch, set_breakpoint,
continue_, pause, step_over, step_in, step_out, stack_trace, variables,
evaluate, threads, terminate` (`tools/debug.rs:70`); `mcp` = `list_servers,
list_tools, call`; `memory` = `learn, recall, list, forget`
(`memory.rs:447-450`); `todo` = `init, start, done, drop, rm, append,
add_dependency, remove_dependency, update_dependencies, view`
(`tools.rs:674-684`); `hub` = `send, wait, inbox, list, jobs, cancel,
read_history` (`orchestration/tools.rs:544`); `goal` = `get, pause,
complete` (`application.rs:3493-3498`).

All extended tools share the same validation contract: arguments are
validated against the schema (`additionalProperties: false` on the
orchestration/todo schemas), capabilities gate execution, and results are
bounded and redacted.

## Capabilities

Each tool declares a `ToolCapability` used by role ceilings and approval
modes: `read` tools (`read`, `grep`, `find`, `glob`, `ls`, `web_search`,
`ast_grep`, `lsp`, `inspect_image`, `hub`, `recall`, `reflect`), `write`
tools (`write`, `edit`, `github`, `memory`, `retain`, `mcp`, `debug`,
`notebook`'s edit action), and `exec` tools (`bash`, `browser`, `eval`,
`notebook`'s execute action, `task`). Unknown or legacy tool metadata
defaults to Exec capability.

## Invariants

- Every tool result is bounded (per-tool byte caps) and passes through the
  secret redactor.
- Session-scoped tools (eval kernels, debug adapter, MCP servers, browser
  processes) are killed on drop — no leaked children, no orphaned
  processes.
- Workspace-relative tools resolve paths through the same containment as
  `read`/`bash` (`resolve_scoped_path`); see
  [`security.md`](security.md).
- Tool arguments are validated against their schemas before execution, and
  unknown actions/arguments fail with actionable messages.

## Related documentation

- [`memory.md`](memory.md) — memory backends
- [`mcp.md`](mcp.md) — MCP client
- [`orchestration.md`](../user-guide/orchestration.md) — `task`/`hub`/`yield`/`goal`
- [`todos.md`](../user-guide/todos.md) — the `todo` tool
