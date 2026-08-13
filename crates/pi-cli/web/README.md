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
$ npm ci             # installs the pinned dependency set (package-lock.json)
$ npm run build      # tsc --noEmit && vite build -> dist/index.html
```

`dist/` is **generated build output and is not tracked by git** (ignored via
the repository `.gitignore`). `crates/pi-cli/build.rs` embeds it into the
binary, so it must exist before the first cargo command: test and release CI
generate it from a clean checkout, and a local source build needs the one-time
`npm ci && npm run build` above. `package-lock.json` is **committed**, so the
frontend build is reproducible from the same locked dependency set CI uses.
Regenerate `dist/` whenever `src/` or `package.json` changes; rebuilding the
Rust binary embeds the new bundle. `npm run build` fails on TypeScript errors.
The released binary embeds the bundle, so runtime and binary-only installs
never need node.

## Development

```console
$ rpi --listen 127.0.0.1:8765        # tokenless loopback — the browser auto-connects
$ cd crates/pi-cli/web
$ RPI_LISTEN=http://127.0.0.1:8765 npm run dev   # http://localhost:5173
```

The vite dev server proxies `/ws` (WebSocket control plane) and `/rpc` to the
listener, so the page behaves like the embedded build. Authentication is
**optional**: a tokenless listener (no `--listen-token-file`) accepts the
browser directly, so the dev page auto-connects with an empty token. To iterate
against the authenticated path instead, start the listener with
`--listen-token-file <workspace>/rpi-token` and enter the token in the auth
field before pressing Connect.

For LAN testing, bind `0.0.0.0:8765` and add `--listen-allow-insecure-remote`,
then open `http://<host-lan-ip>:8765/web` (or any hostname that routes to the
host) from another machine. No `--listen-advertised-origin` is needed for
`/web`, `/ws`, or `/rpc`: the browser is accepted when its `Origin` authority
equals the HTTP `Host` (ordinary same-origin, not authentication and not
DNS-rebinding protection). Plaintext HTTP/WebSocket exposes traffic to passive
LAN observers; it is not a substitute for TLS. Adding
`--listen-token-file` makes the token mandatory on either bind.
`--listen-advertised-origin` is only for collaboration links and the reachable
URL printed at startup.

Alternative for iterating on the *built* page without rebuilding the Rust
binary: `RPI_WEB_DEV_DIR=$PWD/dist rpi --listen ...` serves `dist/index.html`
from disk.

## Security model

- The token travels only in the WebSocket handshake
  (`Sec-WebSocket-Protocol: rpi-auth.<token>`, constant-time compared and
  echoed server-side) and per-listener-authority `localStorage` keys — never in
  URLs or cookies. The legacy global key migrates once to the initial
  authority, then is deleted; switching hosts never reuses another host's
  token.
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
  real Playwright lanes — the generated (gitignored) `dist/` is never modified.