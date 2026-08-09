# Web client (`/web`)

`rpi --listen` serves a web chat client at `GET /web` on the control-plane
listener. The frontend is a React + TypeScript app (`crates/pi-cli/web/`,
built with Vite); `vite-plugin-singlefile` inlines it into one self-contained
`dist/index.html` that `build.rs` embeds into the binary as an asset table
(`path -> MIME/bytes`). At runtime there is no framework, build step, or
external asset — the listener serves the embedded page, and the page drives
the existing [WebSocket control plane](rpc-json.md) in a browser.

Frontend development uses the Vite dev server:

```console
$ rpi --listen 127.0.0.1:8765 --listen-token-file <workspace>/rpi-token
$ cd crates/pi-cli/web
$ npm install
$ RPI_LISTEN=http://127.0.0.1:8765 npm run dev   # http://localhost:5173
```

The dev server proxies `/ws` and `/rpc` to the listener. `npm run build`
regenerates the committed `dist/index.html`; rebuilding the Rust binary
embeds the new bundle. `RPI_WEB_DEV_DIR=<dir>` makes the listener serve a
built page from disk instead, for iterating without recompiling Rust.

## Starting the listener

The web client connects to the control-plane listener that `rpi --listen`
serves. Authentication is **optional**: a tokenless listener accepts browser
connections directly, and a configured token makes authentication mandatory.

**One-command local startup** (loopback, no token — the browser auto-connects):

```console
$ rpi --listen 127.0.0.1:8765
Control plane listening on http://127.0.0.1:8765 (loopback only)
```

Open <http://127.0.0.1:8765/web> in a browser; the page auto-connects with no
token and is ready to use. This is the default for local single-user work.

**Tokenless LAN access** — bind a non-loopback address and opt into
plaintext remote listening (still no token):

```console
$ rpi --listen 0.0.0.0:8765 --listen-allow-insecure-remote
```

Open `http://<host-lan-ip>:8765/web` — or any hostname that routes to the
host — from another machine on the LAN; the page auto-connects with no
token. No `--listen-advertised-origin` is needed for ordinary `/web`,
`/ws`, or `/rpc`: the browser request is accepted when its `Origin`
authority equals the HTTP `Host` — an ordinary same-origin check that
rejects unrelated cross-origin pages, not authentication and not
DNS-rebinding protection. This is plaintext HTTP and WebSocket with **no
authentication and no encryption**: anyone reachable on the network can
drive the agent and observe traffic. Use loopback, or a TLS-terminating
proxy in front of the listener, unless that exposure is explicitly
acceptable.

**Optional authenticated form** — add `--listen-token-file` to make the token
mandatory on either bind:

```console
$ rpi --listen 127.0.0.1:8765 --listen-token-file <workspace>/rpi-token
Control plane listening on http://127.0.0.1:8765 (loopback, authentication enabled)
```

Open <http://127.0.0.1:8765/web>, enter the token, and press **Connect**. For
an authenticated LAN listener, combine the token file with the insecure-remote
opt-in:

```console
$ rpi --listen 0.0.0.0:8765 --listen-token-file <workspace>/rpi-token \
      --listen-allow-insecure-remote
```

The token authenticates clients but provides no encryption: passive LAN
observers can still capture the bearer token and control traffic.

Collaboration join links follow the same bind/advertise separation: wildcard
binds (0.0.0.0 or `::`) require `--listen-advertised-origin <URL>` (a strict
http/https origin without credentials, path, query, or fragment) before
`/collab` — or `collab_start` without an explicit `baseUrl` — can print
reachable links; loopback binds advertise their local address automatically.

The page itself is always served without authentication: it carries no data,
and every command and event flows through the `/rpc` and `/ws` routes, which
are token-gated only when a token file is configured.

## Authentication: the `rpi-auth.<token>` subprotocol

Browsers cannot set the `Authorization` header on `WebSocket` connections.
The listener therefore accepts the token as a WebSocket subprotocol request:

```
Sec-WebSocket-Protocol: rpi-auth.<token>
```

- The offered list is scanned for the first `rpi-auth.<token>` entry whose
  token matches the configured token file, compared in constant time (the
  same comparison used for `Authorization: Bearer <token>`).
- On success the exact offered protocol is reflected in the upgrade response
  (RFC 6455 requires the server to select and echo one subprotocol), so the
  browser accepts the handshake.
- The `Authorization` header path is unchanged. A token file makes the token
  mandatory on every bind; without one the listener is tokenless — browsers
  are accepted on loopback, and on a non-loopback bind with
  `--listen-allow-insecure-remote` when the request's `Origin` authority
  equals the HTTP `Host` (an ordinary same-origin check that rejects
  unrelated cross-origin pages, not authentication and not DNS-rebinding
  protection). Wrong, empty, or whitespace-containing candidates are
  rejected. A configured token authenticates clients but provides no
  encryption against passive network observers.

The token never appears in a URL or cookie; it is held only in the WebSocket
handshake header and kept in `sessionStorage` by the page.

## v1 features

- **Connection state + auto-reconnect** — the status pill shows
  `connecting / connected / reconnecting / offline`; unexpected disconnects
  retry with exponential backoff (1s → 15s cap). A manual **Connect** button
  reconnects after changing the token.
- **Prompt box** — while idle, Enter and the primary **Send** button send
  `prompt`; while a run is active, both switch to **Steer** and send `steer`,
  avoiding a rejected second prompt. Shift+Enter inserts a newline; Esc (or
  **Abort**) stops the active run. Dedicated **Steer** and **Follow up** buttons
  remain available for explicit queue control.
- **Streaming transcript** — assistant turns render live from the event
  stream: text deltas, collapsible `thinking` blocks, tool-call cards with
  arguments and results, bash/tool-result blocks, and final text rendered as
  markdown (headings, lists, code fences, blockquotes, links, images with
  MIME-whitelisted `data:` URIs).
- **Model / thinking switch** — model and thinking-level dropdowns populated
  from `get_available_models` / `get_available_thinking_levels`, applying
  `set_model` and `set_thinking_level`; the session name comes from
  `get_state`.
- **Status line** — a pulsing "streaming" badge while a run is in flight and
  error toasts for failed commands, failed runs, and connection problems.
- **Multi-session authoritative restore** — reloading `/web` restores the
  active session's transcript, goal, todo, and running jobs from the server
  (`get_entries`, `get_state`, `get_tree`) rather than starting empty.
- **Panels** — dedicated views for todo, goal, workflow, session tree,
  settings, subagent jobs, side chat, and maintenance, each driven by the same
  JSONL RPC control plane.
- **Collaboration guest route** — `/collab/ws/<roomId>` serves the same
  embedded client for encrypted live-collaboration guests, reading the
  capability key from the URL fragment locally before opening the encrypted
  WebSocket.

## Security

- Every model-derived string passes through a JavaScript port of the export
  pipeline's `redact_secrets` (credential shapes such as `sk-…`, `ghp_…`,
  `Bearer …`, `token=…`, PEM private keys) before touching the DOM, and every
  string crossing into `innerHTML` is additionally HTML-escaped
  (`& < > " '`). Streaming deltas use `textContent`; model text is never
  injected as raw HTML.
- Links are restricted to `http`/`https`/`mailto` and same-origin relative
  paths; images only render from base64 `data:` URIs with whitelisted MIME
  types.
- The token is never placed in a URL, and there is no cookie, so nothing to
  CSRF. Commands require the token only when one is configured; the page itself
  is static.

## v1 limitations

- Remote approvals and overlay confirmations are intentionally unavailable
  (`extension_ui_response` is hard-rejected on the wire by design); the page
  shows tool results but cannot answer interactive extension prompts.
- No TLS: the listener is plain HTTP/WebSocket. Non-loopback access is an
  explicit plaintext opt-in (optionally authenticated), and passive network
  observers can capture any bearer token and control traffic.

## Testing

- Frontend type-check + build: `cd crates/pi-cli/web && npm run build`
  (`tsc --noEmit && vite build`; the committed `dist/index.html` is what the
  binary embeds).
- Rust: `cargo test -p pi-cli --lib` (subprotocol unit tests) and
  `cargo test -p pi-cli --test listen_control_plane` (GET /web route, positive
  and negative subprotocol auth, existing routes unchanged).
- Browser E2E (playwright-only hard gate): `bash E2E.d/web/run.sh` spawns the
  real binary with the loopback mock provider and runs 11 lanes: core
  (load/auth/stream/abort/todo/rich/workflow/settings/session/subagents),
  goal, xss, abort, reconnect, mobile, auth (tokened listener: no-token
  silent probe, wrong-token error toast, good-token connect), auth_tokenless
  (tokenless listener: empty-token boot auto-connect reaches `connected` and a
  prompt round-trips), and extras/sessions. Most lanes start the listener with a
  token file; the `auth_tokenless` lane starts it without one. Playwright is
  installed ephemerally via npm over a system Chrome/Chromium binary or
  playwright's bundled chromium; a lane with no usable browser driver FAILS —
  there is no skip and no fallback to the `agent-browser` CDP tool.
