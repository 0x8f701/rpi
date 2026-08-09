# Web client E2E regression suite

One command runs the complete web regression suite against the REAL `rpi
--listen` binary (loopback mock provider + real browser):

```sh
bash E2E.d/web/run.sh          # run every lane
bash E2E.d/web/run.sh list     # list the lanes
RPI_WEB_LANES="xss abort" bash E2E.d/web/run.sh   # run a subset
```

Each lane spins up its own fixture — `E2E.d/lib/user_mock_server.py` (loopback
SSE mock) + `rpi --listen` with a token file (`E2E.d/web/lib/fixture.sh`
provides the shared fixture helpers) — opens `/web` in a real browser, and
asserts against the live DOM. The browser driver is playwright (ephemeral npm
install in the scenario work dir) over a system Chrome/Chromium binary or
playwright's bundled chromium. The web lanes are **playwright-only hard
gates**: a missing `node` runtime, a failed playwright install, or no usable
Chromium FAILS the lane (exit 1 = setup failure; exit 2+ = assertion
failure) — there is no skip and no fallback driver. Per-lane pass/fail +
evidence paths are aggregated into `$EVIDENCE_ROOT/web/REPORT.md`; `run.sh`
exits non-zero when any lane failed.

## Feature → lane matrix

| Web feature | Lane | What it asserts |
|---|---|---|
| page load (`GET /web`) | `core` | title, `#conn-state` renders |
| token connect (`rpi-auth.<token>`) | `core` + `auth` | subprotocol handshake; no-token silent probe, wrong-token error toast, good-token connect + round-trip |
| auto-reconnect | `reconnect` | real server kill → `reconnecting` pill → auto-reconnect on the same port (respawn resumes the same session id); pre-crash transcript survives; post-reconnect round-trip |
| model / thinking switch | `switch` | `set_model` + `set_thinking_level` round-trips (dual-model reasoning fixture) |
| streaming (deltas + final) | `core` | slow stream chunks accumulate in the DOM; `stream-badge` clears; fast reply round-trips |
| abort — B1 early-abort preserves streamed text | `abort` | abort on the first delta keeps the partial assistant message; final chunks never render |
| abort — B2 neutral toast | `abort` | abort surfaces `run aborted` with no error styling; no error toast appears |
| markdown (tables / task-lists / fences) | `core` | `table.md-table`, `.md-task-glyph`, no raw ` ``` `/`|---`` leak |
| mermaid SVG | `core` | `svg` inside the assistant message (strict sanitizer) |
| KaTeX math | `core` | `.katex` rendered, math placeholders swapped |
| todo panel (create/complete/live) | `core` | add → complete → reopen via `todo_op`; live counts + detail pane |
| goal panel (create/pin/pause/resume/journal) | `goal` | empty state, create, pin, live pause event from a second WS client, resume, journal replay order |
| workflow panel (create/cancel/live workers) | `core` | create → live status row → cancel → cancelled |
| session panel (new/switch/fork) | `core` | info renders, rename (panel + header), saved list, new session id; session cutover clears the transcript to the empty new-session view (server has no replay) |
| settings panel (browse/edit/apply/secret refusal) | `core` | category browse, secret key redacted + not editable, theme draft → apply → persisted |
| subagents (spawn/live/cancel/output) | `core` | task_spawn → live card + activity → hub_send receipt → job_output pane → job_cancel |
| side chat (multi-tab) | `extras` | default tab, new tab via form, prompt round-trip into the tab transcript |
| maintenance (compact A→B / rewind / handoff / queue) | `extras` | snapcompact `N → M estimated tokens`, rewind list, handoff envelope, queue view + cancel |
| XSS safety (hostile model output) | `xss` | `<img onerror>`/`<script>` render as inert escaped text: no dialog, no `window.__xss`, no injected elements |
| secret redaction | `xss` | `sk-*` credential renders as `[REDACTED]`; raw secret never in the page |
| extension_ui_request approval card | `xss` | fixture QuickJS extension's input hook issues an interactive confirm: card renders hostile title/message as inert text, no toast carries the payload, no error toast, embedded credential redacted |
| WS auth (no/wrong/good token) | `auth` | silent no-token probe, wrong-token error toast, good-token connect |
| mobile viewport (375×667) | `mobile` | core flow at phone width, no horizontal overflow, full-screen drawer, composer above the fold, 44px touch targets, `#thinking-select` hidden, `#sidebar-toggle-btn` visible + clickable and the session sidebar drawer opens |
| multi-session (concurrent runtimes) | `sessions` | **playwright-only** (no agent-browser fallback, no skip): slow A keeps streaming while B's prompt completes; source-session event routing; background unread badge + clear-on-switch; authoritative A/B transcript restore; abort + toast isolation; close busy refusal then idle close success (`loaded` marker drops); 8-session cap with no eviction; Todo/Goal/Workflow never leak across sessions; desktop rail collapse → compact reopen rail; sidebar New/Manage/switch; Android 390×844 drawer opens and closes after a session pick; header has no feature buttons (they live in the sidebar nav) |

## CI wiring

`.github/workflows/test.yml` runs the web suite as a separate hosted gate
(`web client E2E suite` step): node 20 via `actions/setup-node`, then a
playwright warm-up (npm `playwright@1.55.0` + `ws` + `npx playwright install
chromium` in `$RUNNER_TEMP`) followed by `E2E.d/web/run.sh run` with `RPI_BIN`
+ `E2E_CI_WEB=1`. The warm-up and the suite are hard gates: if node, the npm
registry, the chromium download, or any lane's assertions are unavailable or
fail, the job FAILS — no lane skips and nothing falls back.
`E2E.d/ci.sh run` also gates the same runner behind `E2E_CI_WEB=1` for
local/CI reuse.

## Adding a lane

1. Write `E2E.d/web/<name>_test.mjs` (playwright half; exit 2 on assertion
   failure, 1 = setup failure so the lane reports the environment problem
   distinctly) and `E2E.d/web/<name>.sh` (fixture via `lib/fixture.sh` +
   `web_run_playwright`, `list` mode, playwright-only hard gate).
2. Add `<name>` to `LANES` in `E2E.d/web/run.sh` and a matrix row here.
3. New mock behaviors go into `E2E.d/lib/user_mock_server.py` scenarios
   (additive branches only — the steering scenario's odd/even cadence is a
   shared contract the other lanes depend on). The `sessions` scenario routes
   replies by the EXACT current-prompt text (never request numbers — the mock
   counter is global across concurrently-running session runtimes, so numbers
   interleave nondeterministically).
