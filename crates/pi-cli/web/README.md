# rpi web client (`crates/pi-cli/web`)

Vite + React + TypeScript chat client for the `rpi --listen` control plane,
embedded into the `rpi` binary by `crates/pi-cli/build.rs`.

## Stack

- React 18 + TypeScript, bundled by Vite.
- `vite-plugin-singlefile` inlines the JS + CSS into **one** self-contained
  `dist/index.html` (no external assets, no runtime CDN, no build step at
  runtime).
- `crates/pi-cli/build.rs` turns `dist/` into an embedded asset table
  (`path -> (MIME, bytes)`); the listener serves `GET /web` from it.

## Build

```console
$ cd crates/pi-cli/web
$ npm ci             # installs the committed, pinned dependency set (package-lock.json)
$ npm run build      # tsc --noEmit && vite build -> dist/index.html
```

`dist/index.html` is **committed** — it is the pre-embedded artifact, so
`cargo build` (and release CI, cargo-install users, offline builders) never
needs node. `package-lock.json` is **committed** too, so the frontend build is
reproducible from the same locked dependency set that produced the checked-in
artifact. Rebuild `dist/` whenever `src/` or `package.json` changes and commit
the result together with the source change. `npm run build` fails on
TypeScript errors.

## Development

```console
$ rpi --listen 127.0.0.1:8765 --listen-token-file <workspace>/rpi-token
$ cd crates/pi-cli/web
$ RPI_LISTEN=http://127.0.0.1:8765 npm run dev   # http://localhost:5173
```

The vite dev server proxies `/ws` (WebSocket control plane) and `/rpc` to the
listener, so the page behaves like the embedded build. **A token file is
required for the browser**: the control plane deliberately rejects browser
connections on tokenless loopback (browsers always send `Origin`). Enter the
token in the auth field and press Connect.

For LAN testing, add `--listen-allow-insecure-remote` and bind
`0.0.0.0:8765`, then open `http://<host-lan-ip>:8765/web` from another machine.
This authenticated plaintext mode exposes the bearer token and control traffic
to passive LAN observers; it is not a substitute for TLS.

Alternative for iterating on the *built* page without rebuilding the Rust
binary: `RPI_WEB_DEV_DIR=$PWD/dist rpi --listen ...` serves `dist/index.html`
from disk.

## Security model

- The token travels only in the WebSocket handshake
  (`Sec-WebSocket-Protocol: rpi-auth.<token>`, constant-time compared and
  echoed server-side) and `sessionStorage` — never in URLs or cookies.
- Every model-derived string passes through `src/redact.ts`
  (`redactSecrets` + `escapeHtml`, JS ports of the Rust export pipeline)
  before touching the DOM; React's JSX escaping covers the rest. Streaming
  deltas use `textContent`; markdown/tool JSON render through
  `escapeHtml`-first pipelines (`src/markdown.ts`).
- Links are restricted to `http`/`https`/`mailto` and relative paths; images
  render only from MIME-whitelisted base64 `data:` URIs.

## Testing

- Rust: `cargo test -p pi-cli --lib` and
  `cargo test -p pi-cli --test listen_control_plane` (route + subprotocol
  auth contracts).
- Browser E2E: `bash E2E.d/web/run.sh` — the web release lanes are
  **playwright-only hard gates** (no skip, no fallback driver). Each lane
  requires `node`, an ephemeral `npm install` of playwright, and a usable
  Chromium (system Chrome/Chromium on `PATH` or playwright's bundled
  chromium); a missing `node`/`npm`, a failed playwright install, or no
  usable Chromium FAILS the lane (exit 1 = setup failure, exit 2+ =
  assertion failure) and the runner exits non-zero when any lane fails.
  Lanes run against the real binary + loopback mock provider.
- Measured coverage: `bash E2E.d/web/coverage.sh` builds a TEMPORARY
  conditionally-instrumented bundle (`vite.coverage.config.ts`) into the
  evidence root, serves it via `RPI_WEB_DEV_DIR`, and drives it with the
  real Playwright lanes — the tracked `dist/` is never modified.
