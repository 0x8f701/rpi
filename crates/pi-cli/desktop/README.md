# rpi desktop shell

Hosts the rpi listener's `/web` and `/chat` clients. By default it starts and owns a local
release `rpi` process; an explicitly configured runner is connect-only. The renderer has a
restricted native-menu preload bridge, not general Node or filesystem access.

## Prerequisites and launch

Development needs Node/npm, the Electron runtime and its OS libraries, a working graphical
session, and an executable `target/release-dist/rpi` or `target/release/rpi` built with the
current web client. Remote connect-only mode does not need a local runner binary.
Run these commands from `crates/pi-cli/desktop`:

```sh
npm install    # Electron ^44, TypeScript, @types/node
npm run build  # tsc -> dist/
npm start      # electron .; managed local runner by default
npm test       # requires a fresh build
```

With no runner override, the shell selects a loopback port and launches
`rpi --listen 127.0.0.1:<port> --listen-plaintext`, then waits for a compatible `/status`
response before opening `/web` (or the selected `/chat`). It does not adopt a process on an
occupied port. Only readiness `ECONNREFUSED` is retried; other readiness failures are reported.
Use **Runner → Retry connection** after a startup failure.

Explicit runner precedence is `--runner <url>` → `RPI_DESKTOP_RUNNER` → `settings.json`
`runnerUrl`. **Runner → Set runner URL…** persists an explicit connect-only endpoint and stops
any owned local runner. **Runner → Use local runner** drops that stored endpoint and starts the
owned loopback process again. URLs normalize to an origin (`https://` assumed when omitted;
credentials and queries refused; only `/`, `/web`, `/chat` accepted). `--route web|chat`, the
stored route, then `web` select the initial route; the Runner menu switches routes. The shell
loads `<origin>/web` or `<origin>/chat`, never the bare origin. CLI `--runner` and
`RPI_DESKTOP_RUNNER` still override a cleared setting on the next launch. See [`resolveRunnerConfig`](src/helpers.ts) and [`startRunner`](src/main.ts).

Quit waits for the owned child to exit: SIGINT, then SIGKILL after five seconds, with a further
two-second exit deadline. Shutdown failure is reported rather than completing quit. Connect-only
runners are never stopped. Closing all windows quits on non-macOS; on macOS use Quit to stop the
owned runner. See [`DesktopServer`](src/server.ts) and `before-quit` in [`main.ts`](src/main.ts).

## Sign in / configure provider

In managed-local mode, **Runner → Sign in / configure provider…** opens a separate local
authentication window. The native startup-failure dialog also offers this action; it does not
infer an authentication problem from runner stderr or an exit code. Connect-only mode does not
offer local sign-in, and remote runner pages cannot invoke the authentication bridge.

The shell owns one `rpi login --json` child and one authentication window per attempt. Existing
provider selection, text/password prompts, device codes and OAuth instructions come from the
runner's `AuthManager`. Browser links require an explicit click and must be HTTP(S) links from
the current authentication event. The app-owned document uses a separate sandboxed preload,
renders supplied text without HTML interpretation, and clears password fields on submission
and window teardown. Credentials are passed through child stdin, not process arguments.

Only a `complete` frame followed by child exit zero causes one managed-runner startup retry.
**Stored credentials do not prove provider access or a successful model response.** A failed
retry remains visible. Cancel, authentication-window closure, application quit and a transition
to a remote runner cancel the attempt and await its exit. The shell escalates an unresponsive
login child to SIGKILL after 1.5 seconds and reports failure at four seconds if exit is still
unobserved; ownership is retained and another attempt is blocked. Cancellation does not roll
back credentials already persisted by the runner.

The standalone CLI retains `rpi login [PROVIDER] [--scope LABEL]`; `--json` selects a strict
NDJSON exchange instead of terminal prompts. Child frames are `prompt`, `event`, `complete`
and fixed-category `error`; parent commands are `response` with the advertised prompt ID and
string value, or `cancel`. Select responses must match an advertised option ID. Frames are
bounded to 1 MiB, and malformed input, unterminated EOF or cancellation abort the attempt.
The current nonblocking inherited-stdio transport is Unix-only; unsupported platforms fail
rather than falling back to terminal interaction. See [`login.ts`](src/login.ts),
[`login-preload.ts`](src/login-preload.ts) and [`auth_json.rs`](../src/auth_json.rs).

## Native actions

[`preload.ts`](src/preload.ts) exposes only `rpiDesktop.onAction` and `setActions` to the configured
runner's top-level frame. Main validates the sender, frame, origin and five allowed action kinds;
menu items stay disabled until the current page advertises a handler, and reset on navigation.
There is no arbitrary IPC, command execution or native file-path API.

- `/web`: New workspace and New session open the session panel's working-directory input;
  Settings opens settings; Attach files requires a session; Stop generation requires an armed stop.
- `/chat`: New workspace opens the project form; Settings targets the focused runner or selected
  project. A mounted session view supplies Attach files when its composer is enabled and Stop
  generation when connected and armed. Native New session is not registered and stays disabled;
  use the existing in-page session flow.
- Pages without this bridge integration retain disabled native actions. Edit, View and Runner
  menus remain shell-owned. The renderer uses its existing file input and stop/settings handlers.
  Native Attach preserves Chromium user activation through a fixed, origin-checked event;
  the existing renderer file input still owns selection and attachment intake.

Sources: [`desktopBridge.ts`](../web/src/desktopBridge.ts), [`App.tsx`](../web/src/App.tsx),
[`ChatApp.tsx`](../web/src/chat/ChatApp.tsx), [`SessionView.tsx`](../web/src/chat/SessionView.tsx).

## Packaging

`bash crates/pi-cli/desktop/package-deb.sh [output-dir]` builds an amd64 or arm64 `.deb` for the host Debian architecture, using host Debian
packaging tools (including `dpkg-deb`, `dpkg-query` and `ldd`), Node/npm and standard shell utilities;
no Electron packaging framework is added. Install desktop npm dependencies and build the current
web client and release runner first. The script rebuilds the desktop TypeScript, then copies
`target/release-dist/rpi` (or `target/release/rpi`) into `resources/rpi` alongside Electron and the app
under `/opt/rpi-desktop`. Packaged startup uses only that bundled binary, with the user's home as
its working directory; development runner startup uses the launching working directory.

The package adds `/usr/bin/rpi-desktop`, a desktop entry and an icon. There are no maintainer
scripts. Electron-linked library dependencies are derived from the build host unless `DEPENDS`
is supplied. Directories are 0755, executables including the runner are 0755, `chrome-sandbox` is
root:root 4755, and other files are 0644; the script audits payload modes and rejects non-regular
entries. It does not ship `--no-sandbox`. Output defaults to `dist-deb/` (gitignored).
See [`package-deb.sh`](package-deb.sh).

`VERSION=… RPI_BINARY=… bash package-mac.sh out.zip` and `package-win.ps1` produce unsigned macOS and Windows zip archives with the host Electron runtime and the matching `rpi` or `rpi.exe` under the app resources. The tag workflow publishes those archives together with the Linux packages. None of these desktop packages are code-signed or notarized.

## Certificate trust

A remembered certificate exception, not identity pinning: Chromium refuses an untrusted listener
certificate until the user decides, and **Trust and reload** remembers its fingerprint for that
exact origin (scheme, host, port) in `trusted-pins.json`. No certificate verify proc is registered;
trust is granted through `certificate-error` only. Chromium-valid replacement certificates are not
challenged by this mechanism. Chromium may cache accepted verification; prompt frequency and whether
the request completes immediately or needs a reload remain unverified. An unreadable pin file is
reported and never overwritten.

## Security posture

`contextIsolation`, `sandbox` and `webSecurity` are on; `nodeIntegration` is off. Main-frame
navigation is runner-origin only; subframes may also use `about:blank`/`about:srcdoc` and `blob:`
with the runner's embedded origin. Other origins, including `data:`, are refused for page-initiated
navigation. Permissions (`notifications`, `clipboard-sanitized-write`, audio `media`; camera refused)
are runner-origin only; permission checks without a media subtype cannot distinguish audio/video.
CSP is injected on document responses only: `/web` gets `connect-src 'self'` plus its own `ws(s)`
origin, while `/chat` permits `http(s)`/`ws(s)` to user-added runners. This does not bypass TLS,
CORS, mixed-content or OS permissions. Remote HTTP does not gain secure-context browser APIs.
Off-origin redirects are refused and reported; window-open requests for HTTP(S)/mailto require
consent. Off-origin downloads are refused; same-origin downloads retain Chromium's save workflow.
Failed main-frame loads render a local error page (cancelled-load `ERR_ABORTED` is excluded).

## Acceptance status

Full desktop UI and packaged acceptance remain **PENDING**. A graphical Electron `/web`
journey verified native File → Attach files opening the GTK chooser, selecting a 33-byte fixture
into exactly one composer attachment, and reopening the chooser. Existing Submit rendered the
fixture/message but the provider returned HTTP 402; a successful model response was not verified.
Native Attach was disabled on `/chat` home without a session. These limited checks do not establish
all native-menu journeys, packaged install/launch or quit-ownership acceptance. Earlier headless
Electron launch attempts exited 1 (**FAILED**); they remain failed attempts, not graphical proof.

Fresh empty-config native onboarding through the startup-failure sign-in action remains
**PENDING**. Authored protocol/security tests and successful compilation are not native UI or
provider-access evidence. A provider-selection-and-cancel journey, when exercised, establishes
only that entry and cancellation path, not credential persistence or successful model access.
