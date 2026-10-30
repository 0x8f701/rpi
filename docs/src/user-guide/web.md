# Web client (`/web`)

The Web client controls sessions through the same application command layer as the terminal interface. Open `/web` from an interactive session, or start a standalone listener:

```sh
rpi --listen 127.0.0.1:8543
```

The standalone listener runs independently of standard input and does not start the terminal interface. It serves HTTPS by default, using an automatically generated certificate when no certificate pair is supplied. Use `--listen-cert` with `--listen-key` for your own certificate. `--listen-plaintext` explicitly selects HTTP.

## Connection and pairing

Use the address printed by the listener. The Web startup banner supplies a token-bearing pairing URL when authentication is configured; treat that URL as a credential and do not share it publicly.

When the listener requires a token and none is stored, the client enters `token_required` rather than opening an endless unauthenticated reconnect loop. The pairing card identifies the listener and accepts its access token. A nonempty submission saves the token for that listener and reconnects. Settings also provides connection configuration.

For remote access, use TLS and `--listen-token-file`. A non-loopback listener requires a token unless `--listen-allow-insecure-remote` explicitly opts out. A configured token remains mandatory even with that opt-in. Wildcard binds require `--listen-advertised-origin` so generated links have a usable origin.

## Sessions and conversation

The session list separates saved session selection from the displayed runtime. Prompts, streamed replies, tool output, approvals, and session state belong to the selected session. Switching the displayed session is not a command to stop another session.

The client includes session, model, settings, Todo, subagent, activity, workflow, file, automation, and process surfaces. Availability depends on the active session and configured capabilities. Use **Stop** to interrupt generation; the underlying protocol identifier remains `abort`.

**Steering queue status: in progress.** The Web queue component provides queued-prompt previews. The Rust queue and scoped cancellation contract are still being integrated; do not rely on `QueueCancel` scopes `all`, `steering`, or `index` as a stable end-to-end interface yet.

## Loop

The Loop panel is the Web counterpart of `/loop`. It creates, lists, and deletes recurring prompts through `loop_create`, `loop_list`, and `loop_delete`.

- Interval presets: **30s, 1m, 5m, 15m, 1h, 4h**.
- The initial interval selection is **5m**.
- Creating a loop sends the trimmed prompt with `fireImmediately: true` and `durable: false`.
- Each row shows its interval and a prompt preview. Cancel removes that loop and refreshes the list.

Recurring prompts are separate from supervised operating-system processes.

## Processes

The Processes panel provides `/ps`-style supervised command management:

1. Enter a command and an optional label.
2. **Run** submits `process_spawn` with `argv: ["bash", "-c", command]` and the session working directory. An unknown working directory fails immediately rather than running elsewhere.
3. **Output** opens a per-process output viewer using `process_logs`. The client requests a 16 KiB tail, decodes base64 output as UTF-8, and prefixes reported output loss with `(lost N bytes)` when `lost` and positive `lostBytes` are returned.
4. **Stop** sends `process_stop`. Supervision uses termination followed by a grace period and forced kill when necessary.

While the panel is mounted, process state and the open output view refresh every three seconds, except while a panel action is busy. Closing the panel stops polling; it does not itself stop a process.

## File explorer

The Files surface offers a workspace tree, changed-file view, and file contents. Previewable files start in **Preview**; **Source | Preview** switches between rendered content and the line-numbered source surface. A directory that cannot be listed — deleted, permission-denied, or a symlink — is hidden from the tree rather than shown as an error; changed files beneath it that exist only in the changed-file catalog still appear under the parent directory.

| Extension | Preview |
| --- | --- |
| `.md`, `.markdown` | Shared safe Markdown renderer |
| `.html`, `.htm` | Sandboxed iframe with `sandbox=""`; scripts are not allowed |
| `.svg` | Image element rather than inline executable markup |
| Other files | Source surface |

Directory listings and changed-file catalogs are capped at **5,000** entries. The client also caps rendered changed-file rows at **5,000**, and reports truncation. These are safety bounds, not a promise that a large workspace is fully represented in one response. The project's frontend policy requires generous limits sized for heavy normal workflows.

The static Files header keeps refresh, fullscreen/restore, and close separate from aligned per-column toolbars: Search and tree visibility belong to the tree column; **Source | Preview | Ask | Map** belongs to the file-content column. The collapsed tree retains a reachable toggle. Selected-line actions and Blame results float over the surface without inserting rows or shifting the file content. File and folder types use SVG icons, including Solidity. Search and the open file conversation survive tree visibility changes. Fullscreen hides the global header until Files is restored or closed.

**Ask** adds a per-file conversation to the open file tab. Questions stream into the dock with a ■ Stop control, and a selection on the numbered source lines attaches that inclusive line range as context. The range stays attached after the painted page window compacts or pages away; the backend reads those exact lines (at most **2,000**), and the snapshot reports what was attached (`requestedRange` / `includedRange` / `truncated`). A wider selection is rejected. Switching session owner or file, or leaving file view, drops the selection. Conversations run on a read-only fork, so their tool calls stay in the dock and never enter the main transcript. Each tab owns one conversation (up to 8 open tabs); hiding the panel keeps drafts, selections, and snapshots, while closing the tab ends that conversation and a reopened tab starts a new one.

### Ask or blame selected source lines

Select numbered lines on the **Source** surface, then choose **Ask** or **Blame**. Both actions use the same inclusive, 1-based range, with a maximum of **2,000 lines**. These selection actions are not available on diff or rendered Preview content. Selected **Ask** opens the existing file conversation and focuses its composer without replacing your draft or conversation history.

**Blame** shows attribution for each selected line: commit, author, and date (the response carries Unix-seconds time). Uncommitted changes have a null commit and display **Uncommitted**, rather than a zero commit hash. The request is scoped to the selected file and exact line range; the backend validates source access and uses bounded, read-only Git commands with explicit arguments, not a shell (`crates/pi-cli/src/file_explorer.rs:915`).

The conversation shows only the question you typed, not the injected source context; the model still receives that context (`crates/pi-cli/src/file_conversation.rs:411`). Ask's isolated side-chat session preserves the parent session's already-authorized workspace roots, so authorized additional folders remain readable. Tool instructions reflect the tools actually installed in the child session, not shell examples inherited from the parent. This does not grant `BashExec` or expand Ask beyond its read-only tools (`crates/pi-cli/src/side_chat.rs:349`, `:1633`).

### Dependency Map

**Map** shows the selected file's bounded one-hop incoming and outgoing dependencies, with navigable file nodes and source-documentation evidence. It extracts TypeScript/JavaScript, Rust, and Solidity relationships, but does not claim complete resolution. Relative TypeScript/JavaScript imports and exact Solidity source-unit paths are resolved conservatively. Rust module ancestry, attributes, and crate bindings remain explicitly **unresolved**, not inferred from filenames.

Read the coverage notice: truncated scans, unsupported or unreadable files, incomplete parses, and omitted dynamic imports can leave relationships unknown. Ignored files and symlinks are outside the visible-entry scan; an empty incoming list does not prove that a file has no dependents (`crates/pi-cli/src/dependency_map.rs:1`, `crates/pi-cli/web/src/panels/FileDependencyMap.tsx:96`).

## Commit review

`/commit` reviews a commit without touching the worktree: bare `/commit` opens a recent-commit chooser (newest first, first-parent history, at most 100 rows), and choosing a row or entering `/commit <ref>` reviews that commit pinned to its resolved hash — a root commit diffs from the empty tree, a merge from its first parent. Nothing stages, checks out, or commits.

### Focused Auto Review and Highlight

**Auto Review** screens for demonstrated critical bugs, overdesign that violates the owning project's principles, and naming violations supported by its rules. It does not request generic warnings, style preferences, speculative risks, or change summaries. Naming-only and rename-only changes remain eligible for review, along with policy-relevant comment, test, and abstraction changes (`crates/pi-cli/src/code_review_panel.rs:502`, `:978`).

Auto Review and Highlight share one structured `reviewFindings` collection. Each hunk result has an exact `identity` and a `findings` list. Each finding records **Critical**, **Overdesign**, or **Naming**, an **old/new** side, inclusive 1-based `startLine`/`endLine`, a `summary`, and supporting `evidence`. Multiple findings in one hunk remain distinct. **Clean** is derived from an explicitly reviewed empty list, not a finding category or separate verdict. Structured model output uses `{"hunks":[{"identity":...,"findings":[...]}]}`; ranges must match actual numbered rows on the stated side, not merely fall within the hunk header. Legacy severity categories are not accepted.

Finding marks identify the relevant source lines. **Previous/Next finding** moves within and across files in file/hunk/physical-row order without wrapping. With no selection, Next chooses the first finding and Previous the last. Selecting one opens its file, reveals the paged hunk and exact line range, and synchronizes the detail. Replacing findings invalidates old selected references. Automatic finding details do not replace your manual hunk conversation or draft.

After the existing **151ms** busy gate, the Web panel shows **Screening…** while Auto Review selects hunks, **Review N/M** for completed/submitted deep reviews, and **Identifying…** while Highlight runs. Unknown review counts appear as **Review --/--**, not invented progress (`crates/pi-cli/web/src/codeReview.ts:3258`).

One-shot screening and Highlight keep the project's system policy but omit unrelated main-conversation history. Manual hunk conversations and deep-review histories are unchanged (`crates/pi-cli/src/code_review_panel.rs:704`, `:1132`). This is a context and progress-display change, not a measured speed claim: no post-change timing comparison is available.

### TL;DR summary

**TL;DR** generates a summary owned by the current review snapshot. The pane opens expanded; **Collapse details** and **Expand details** render the same document through the existing safe Markdown body, including supported Mermaid rendering. **Stop** cancels the summary operation, not the review or a manual hunk reply; **Close** only hides the summary pane.

The latest integrated runtime, device, and deployment acceptance for these changes remains pending. These descriptions document implementation, not a claim that the latest build has been deployed or runtime-verified.

## Rendering and security

Markdown escapes input before applying supported formatting: raw HTML is never rendered as document markup. Mermaid uses strict security and sanitized SVG. HTML file preview is a deliberately separate sandboxed surface, not an exception to the Markdown rule.

Web access grants control over an agent running with the host's configured authority. Pair only with listeners you trust, protect token files and pairing links, and review tool approvals. See [Security](../reference/security.md) and [RPC protocol](rpc-json.md).
