# E2E scenarios (user-perspective tmux tests)

End-to-end (E2E) scenarios exercise rpi from the operator's interface. Terminal scenarios use real terminal interaction; Web scenarios use a real listener and browser. A protocol-only assertion does not establish that a visible control works.

## Discover suites

```sh
bash E2E.d/features.sh list
bash E2E.d/features.sh
bash E2E.d/list.sh
bash E2E.d/web/run.sh list
```

`E2E.d/features.sh` is the inventory runner: one command exercises the `FEATURES.md` map by orchestrating crate `--release` tests, CI campaigns, TUI tmux scenarios, collab, listen smoke, and the Web Playwright suite. `E2E.d/list.sh` still lists each suite separately. Use these generated lists instead of relying on a static lane inventory.

The Web runner prepares the current-tree Web bundle and Playwright/Chromium once. Ordinary independent lanes use bounded concurrency; coverage, pressure, stress, and resource-sensitive lanes remain serial. Each lane owns its fixture, listener, token file, and browser.

## What the suites cover

The registered Web scenarios include conversation round trips, interruption and recovery, model selection, authentication, tokenless connection, session routing/restoration, workflows, Goals, Todos, rich Markdown, hostile markup, scroll pinning, reconnect behavior, and mobile layouts. Individual lane coverage is defined by the current runner and scenario assertions, not by this summary.

A live-model scenario is distinct from a deterministic scenario with a local provider fixture. Both can exercise the real client; only the former establishes behavior against a live provider.

## Evidence rules

- Execute release or release-dist artifacts, never debug-profile product binaries.
- Capture the actual terminal interaction or browser journey.
- Inspect screenshots for layout and state; the presence of a screenshot filename is not verification.
- Preserve exact command, exit status, and failure output.
- An authored but unexecuted assertion is pending, not passed.
- A failed scenario is failed even if another narrower check passes.
- Do not run broad suites while another writer is editing participating files.

For Web changes, exercise the actual click/type/scroll/close path at desktop and mobile sizes. Verify that closing a panel leaves the header and composer usable. Authentication checks must distinguish missing, wrong, and accepted tokens without recording their values.

This page describes available verification infrastructure; it does not claim that the current working tree has passed these suites.
