# Web client E2E regression suite

One command runs the complete web regression suite against the REAL `rpi
--listen` binary (loopback mock provider + real browser). The suite is
*29 lanes — `core goal xss abort scroll reconnect readygate switch mobile auth
auth_tokenless extras sessions session_restore projects commands_review
loop_goal attachments slowclient external_sessions presentation realtime_webrtc
readygate_spawn code_review_paging skill_completion realtime_rpc appborder
personas stt_rpc`
(mirroring `LANES` in `E2E.d/web/run.sh`):

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
gates**: a missing `node`/`npm` runtime, a failed playwright install, or no
usable Chromium FAILS the lane (exit 1 = setup failure; exit 2+ = assertion
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
| transcript scroll pinning | `scroll` | **playwright-only**: long stream stays bottom-pinned (scrollTop tracks scrollHeight, remaining distance never exceeds the tolerance); a manual scroll away unpins and incoming deltas preserve the viewport EXACTLY while the transcript keeps growing; scrolling back to the bottom re-pins; switching back to a long, previously unpinned session pins its transcript to the bottom intentionally |
| markdown (tables / task-lists / fences) | `core` | `table.md-table`, `.md-task-glyph`, no raw ` ``` `/`|---`` leak |
| mermaid SVG | `core` | `svg` inside the assistant message (strict sanitizer) |
| KaTeX math | `core` | `.katex` rendered, math placeholders swapped |
| todo panel (create/complete/live) | `core` | add → complete → reopen via `todo_op`; live counts + detail pane |
| goal panel (create/pin/pause/resume/journal) | `goal` | empty state, create, pin, live pause event from a second WS client, resume, journal replay order |
| workflow panel (create/cancel/live workers) | `core` | create → live status row → cancel → cancelled |
| session panel (new/switch/fork) | `core` | info renders, rename (panel + header), saved list, new session id; lifecycle responses restore the target session from the authoritative backend snapshot |
| settings panel (browse/edit/apply/secret refusal) | `core` | category browse, secret key redacted + not editable, theme draft → apply → persisted |
| subagents (spawn/live/modal/cancel/output) | `core` + `web-subagents` | task_spawn → live card + activity → running-detail modal (dialog a11y, task/status/elapsed/activity, non-empty recent history, Refresh, Escape/Close) → hub_send receipt → job_output pane → job_cancel |
| side chat (multi-tab) | `extras` | default tab, new tab via form, prompt round-trip into the tab transcript |
| XSS safety (hostile model output) | `xss` | `<img onerror>`/`<script>` render as inert escaped text: no dialog, no `window.__xss`, no injected elements |
| secret redaction | `xss` | `sk-*` credential renders as `[REDACTED]`; raw secret never in the page |
| extension_ui_request approval card | `xss` | fixture QuickJS extension's input hook issues an interactive confirm: card renders hostile title/message as inert text, no toast carries the payload, no error toast, embedded credential redacted |
| WS auth (no/wrong/good token) | `auth` | silent no-token probe, wrong-token error toast, good-token connect |
| tokenless loopback listener | `auth_tokenless` | empty-token browser bootstrap reaches connected and a prompt round-trips without weakening the non-loopback authentication policy |
| mobile viewport (375×667) | `mobile` | core flow at phone width, no horizontal overflow, full-screen drawer, composer above the fold, ≥44px touch targets, dominant ≥240px textarea, one unified Send/Stop action, hidden `#thinking-select`, and a working session drawer toggle |
| multi-session (concurrent runtimes) | `sessions` | **playwright-only** (no fallback/skip): slow A continues while B completes; source-session routing; unread clear-on-switch; authoritative A/B restore; abort/toast isolation; 8-session cap; Todo/Goal/Workflow isolation; desktop rail and Android drawer behavior |
| session persistence and restore | `session_restore` | Web prompt records normally; loaded switch restores backend history; listener SIGTERM/restart re-binds to the new primary and restores the persisted session; a page reload restores the last-activated session from the per-authority preference (R5); a missing saved id falls back to the first catalog row (R6) — R5/R6 executed-assertion evidence written to `coverage-assertions.json` like the sessions lane |
| all-project session catalog + cross-project New-session storage | `projects` | **playwright-only**: two valid native sessions (version-3 JSONL) for two temp project cwds are seeded under ONE isolated profile native tree (`<agent>/sessions/--<encoded-cwd>--/`); the real listener starts from project A; the sidebar groups BOTH projects under the rpi provider (no tmp/UUID/source top-level groups); the Session panel shows backend cwd/project A; switching to project B activates backend cwd B; New session inherits B and its file lands under B's ENCODED default session dir (browser-visible path + on-disk header/`cwd` proof, no new file under A) |
| external sessions (OMP/Codex/Grok discovery + secure import) | `external_sessions` | **playwright-only**: seeds OMP/Codex/Grok foreign sessions under the isolated HOME; Web default discovery lists and groups them; clicking a foreign Codex row transparently activates an rpi native `import_*.jsonl` copy (panel path under the native tree); foreign source bytes + mtime stay unchanged; re-select reuses the same native copy (no second import); no duplicate imported/foreign logical row; explicit `sessionImportSources: []` is native-only |
| composer command button + code-review panel | `commands_review` | **playwright-only**: `#command-btn` is left of `#prompt-input`; the command picker lists `/compact` `/skill` `/code-review`; choosing `/code-review` inserts the draft WITHOUT auto-submitting; Enter opens the real review panel rendering the HEAD→working-tree comparison label, the dirty file row, and the changed diff lines (deletion + addition); closing removes `#code-review-panel`; `/skill <fixture>` renders the loaded skill's frontmatter summary visibly; `/compact` dispatches the compact RPC observed on the outgoing WS frame (provider round-trip not required) |
| composer `/loop` + `/goal` + `/ps` commands | `loop_goal` | **playwright-only**: the command picker lists `/loop` + `/goal` + `/ps` (get_commands authority); choosing them drafts `/loop ` / `/goal` / `/ps` WITHOUT auto-submitting; Enter dispatches the typed RPCs against the real listener — loop create/list/update/delete/cancel (create/update rejected locally with the TUI-equivalent `/loop is unavailable while another turn is running` toast while a turn streams), goal create/show/pin/pins/pause/resume/complete/drop (create/resume dispatch `activate: true` and render `Goal work started|queued|already active · {state}`; drop after complete surfaces the invalid-transition error), and bare `/ps` (process_list resolves `No supervised processes` in the empty fixture); malformed args fail locally with TUI-equivalent usage toasts and dispatch no RPC (incl. `/ps extra` on the bare-only surface); intercepted commands never create user bubbles; every frame is sessionId-stamped (no cross-session routing) |
| composer attachments | `attachments` | **playwright-only**: image-file paste intercepts only file clipboard payloads; ordinary text paste remains native; multi-file `.rs`/`.ts` picker and drag/drop preserve global intake order; oversized/unsupported PDF is rejected; the outgoing prompt frame contains the bounded image/code blocks in order and clears attachments after dispatch |
| slow-client and command lifecycle | `slowclient` | transient event bursts and controlled main-thread stalls do not trigger false 1008 disconnects; sustained unread clients still close with the real policy reason; pending commands settle with the truthful close cause rather than timeout/replacement mislabels |
| user presentation (tool cards / thinking / composer / session / media) | `presentation` | **playwright-only**: real-browser DOM assertions (no source-text checks) for Command card title "Command" + two-line clamp + no raw args + success-green/failure-red computed border; process/write/read human summaries with no raw JSON; all tool cards equal width; process `.op`-missing error card; `#command-btn`+`#prompt-input`+`#send-btn` equal height (desktop) + buttons equal height + textarea≥240 (mobile); thinking visible during streaming then absent after final; durable bashExecution via RPC `.msg--bash`; session sidebar top-level providers only rpi/Codex/Grok/OMP (no tmp/UUID) + search filter/clear; inline image `naturalWidth>0`; video `controls`+`preload=metadata`+no autoplay; hostile media (unsupported MIME) renders no media node |
| real WebRTC (real RTCPeerConnection loopback) | `realtime_webrtc` | **playwright-only**: two REAL chromium `RTCPeerConnection`s (no FakePeerConnection) + the real `src/realtime.ts` helpers bundle: getUserMedia → createOffer → the ICE-gather wait flips `bareOfferHadCandidates` false→`gatheredOfferHasCandidates` true, real SDP exchange, `oai-events` datachannel round-trip, remote audio track arrival, `classifyRealtimeConnectionState` connected; loopback-2 drives `setupRealtimeCall` end-to-end and asserts `session.update` (V1 quicksilver shape, voice under `audio.output.voice`) rides the datachannel — listener installed synchronously in `ondatachannel`, so the caller's `dcopen` send cannot race it |
| WS delayed-open/reconnect sidebar+panel loads | `readygate_spawn` | **playwright-only**: `sessions` scenario; boot sidebar lists the primary row; a REAL server kill enters `reconnecting` and the sentinel `load failed: not connected` never appears anywhere (body/`.session-sidebar__error`/`.panel__status`) during a ≥12s outage crossing the sidebar's 8s poll; same-port respawn returns to `on` and the sidebar + session panel reload their rows WITHOUT user action (gated poll resolves on `notifyOpen`); sentinel absent post-reconnect |
| code-review tree / diff paging / comment markdown | `code_review_paging` | **playwright-only**: nested file tree (`li.code-review__tree-row[data-tree-kind="dir"]` + `button.code-review__tree-dir`, `aria-expanded` on the li) collapses/expands hiding/restoring child file rows; the >4000-line fixture renders the truncation banner + first window ≤4000 lines; `button.code-review__load-more` grows the window so `changed-line-04001` appears; `button.code-review__load-full` reaches the total or the hard cap; a comment carrying `**bold**`, a list, a ` ```rust ``` ` fence and hostile `<script>`/`<img onerror>` renders markdown (strong / ul>li / `pre.md-fence__pre > code.hljs` + `.md-fence__lang`) while the hostile HTML stays LITERAL text (no element, no `window.__crPwned`, no dialog) — for the user comment AND the mock-routed assistant reply |
| `/skill` candidates from the real loaded catalog | `skill_completion` | **playwright-only**: two-phase — with-skills seeds `.pi/skills/greet/SKILL.md` + `.pi/skills/docs/SKILL.md` and asserts BOTH render as `.command-picker__option[data-skill-name]` rows with non-empty frontmatter descriptions (candidates come from `get_commands`, never a hardcoded list); selecting greet inserts `/skill greet` without auto-submitting and Enter renders the frontmatter summary bubble (`div.msg.msg--summary`, label "skill"); no-skill workspace shows `.command-picker__hint` "No skills loaded" with zero candidates |
| realtime start/error/stop RPC lifecycle | `realtime_rpc` | **playwright-only**: live settings advertise realtime mode; clicking `#mic-btn` dispatches `realtime_start` on the WS and the REAL Rust proxy reaches the mock (evidence records `OpenAI-Alpha: quicksilver=v2` + Bearer); the overlay (`#realtime-transcript`, label "realtime voice", live dot, `#realtime-conn-state` bucket) renders; with the mock rejecting (500) the page surfaces the `realtime call failed` toast and the overlay stays down; clicking again dispatches `realtime_stop` and the overlay disappears (in-page RTCPeerConnection stubbed for determinism; transport covered by `realtime_webrtc`) |
| desktop app-main panel border + mobile overflow | `appborder` | **playwright-only**: computed styles — desktop dark AND light `.app-main` `border-left/right` are `1px solid` resolving to the theme's `--border-strong` token (tracked via a probe element, never a hardcoded hex), `header` border-bottom + `.app-main > footer` border-top are the same edge, `.session-sidebar` has no border-right (no doubled seam), `#transcript` background == `--bg` with `scrollbar-gutter: stable`; the rail-collapse toggle keeps the edge; mobile (390×844) drops both edges (`border-left/right-style: none`) with zero horizontal overflow (`documentElement.scrollWidth <= clientWidth`, tolerance 0) |
| persistent persona definitions | `personas` | **playwright-only HARD GATE**: seeded durable persona (persona.md + memory + sessions) listed with memory/session counts and NO absolute paths in the panel DOM; definition view modal (dialog a11y, literal persona.md content, persistence semantics); select-as-preferred marker; Run dispatches `task_spawn` with the persona AGENT name (job card bound to `(mentor)` in the Subagents panel); create makes the catalog discoverable after the config save; edit name-agreement gate blocks mismatched frontmatter names; remove-vs-purge confirmation dialog keeps the two containment semantics distinct and the REAL fixture filesystem proves remove keeps `memory/`+`sessions/` while purge deletes the root; DOM hygiene (no mock credentials, no absolute fixture paths) |
| hold-to-talk STT via backend proxy | `stt_rpc` | **playwright-only HARD GATE**: live settings advertise STT mode; a synthetic-mic hold dispatches `stt_transcribe` on the WS with ONLY `{audioBase64, mimeType: "audio/wav"}` (no URL, no key — the browser never holds the endpoint or credentials); the REAL Rust proxy reaches the mock `/v1/audio/transcriptions` with the server-held Bearer + multipart WAV + model (evidence records `authPresent`/`contentType`/file/wav/model metadata only — never the key or the audio body); the returned transcript lands in `#prompt-input`; the fixture key appears nowhere in the DOM or evidence; with the mock rejecting (500) the page surfaces the bounded `transcription failed` toast and no transcript lands |

## Focused lanes

`web-subagents` (`subagents.sh` → `subagents_test.mjs`) is a standalone,
skip-guarded lane NOT in `LANES`/`run.sh`: same steering + orchestration + writer
fixture as `core`, but only the Subagents acceptance, exercised with a wider
(~16s) `web-e2e-subagent` running window so the running-job detail modal can be
opened, refreshed, and closed against a genuinely in-flight child. It is
standalone evidence; the `core` hard gate carries the same modal assertions for
CI. It SKIPs (never fails) when `node` is missing.

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

## Measured coverage

`bash E2E.d/web/coverage.sh` measures real line/function/branch coverage of
`crates/pi-cli/web/src/**/*.{ts,tsx}` through the REAL `rpi --listen` binary +
loopback mock + REAL Playwright assertions, and enforces explicit Istanbul
thresholds. Every step is a hard gate — no skips, no agent-browser fallback:

1. Build a TEMPORARY conditionally-instrumented bundle
   (`vite.coverage.config.ts`: inline source map, unminified) into the
   evidence root (`$EVIDENCE_ROOT/coverage/web-coverage-dist`) — the generated
   (gitignored) `dist/` is never modified; the bundle is served via `RPI_WEB_DEV_DIR`.
2. Verify playwright installs and that chromium actually launches (missing
   `node`/`npm`, a failed playwright install, or no usable Chromium FAILS the
   run).
3. Run the matrix driver (`coverage_test.mjs`) against the steering fixture —
   including a REAL server kill/respawn reconnect — and the XSS matrix driver
   (`coverage_xss.mjs`) against the xss scenario + approval extension, the
   fallback matrix driver, and the collaboration browser guest driver.
4. Run the REAL web lane suite (`E2E.d/web/run.sh`, every lane) against the
   same coverage bundle — each lane must pass AND must produce a coverage
   payload (a lane that skipped or fell back fails the coverage run).
5. Merge every V8 payload, convert through the inline source map
   (`crates/pi-cli/web/scripts/coverage-report.mjs`), verify source mapping
   for every expected `src/` file, emit text + JSON summary + lcov, enforce
   the global thresholds, and enforce `src/scrollPin.ts` at ≥90% for lines,
   functions, branches, and statements.
6. Validate the feature matrix against the executed assertion evidence from
   the drivers plus sessions, restore, projects, scroll, external-session, and
   presentation lanes (`coverage_matrix.mjs`) — zero uncovered required assertions.

The packaged `dist/index.html` is rebuilt by the normal `npm run build` step;
`coverage.sh` never writes `dist/`.

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