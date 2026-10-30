# Security

rpi can read files, run tools, call remote providers, load extensions, and expose a network control plane. Protect each boundary separately. Project trust, tool approval, sandbox confinement, and safe rendering are not substitutes for one another.

## Project resources and host authority

Project settings and executable resources require admission through the trust boundary. `--approve` and `--no-approve` are per-run project decisions. Tool approval uses the separate `yolo`, `write`, or `ask` policy. Host trust hooks run before loading project extensions.

Project context files use strict no-follow reads: a symlinked `AGENTS.md` or `CLAUDE.md` is rejected even when the project is trusted. Global managed-resource links have a separate capability policy. File operations retain capability authority and validate path/identity rather than trusting a displayed pathname.

CLI `-C` never creates a missing working directory. An explicit absolute `cwd` for `new_session` is auto-created when missing, including nested directories, through capability-walked no-follow creation. Symlinks, malformed paths, and file conflicts fail closed; retained directory identity is validated before use.

## Process and network confinement

Linux confined modes use Bubblewrap and macOS uses Seatbelt. Windows does not implement confined modes. `danger-full-access` executes with host authority and must not be described as sandboxed.

Long-lived commands must be supervised with `background=true` or the process tool. Unsupervised shell background/detach syntax remains rejected. Stop uses termination, a grace period, and forced kill when required. Shell `grep` and `rg` are allowed by bash; the preference for dedicated search tools is prompt guidance, not a security parser guard.

Read [sandbox confinement](sandbox-isolation.md) before changing policy to solve an execution error.

## Web listener authentication

The standalone listener uses HTTPS unless `--listen-plaintext` is explicit. Without a supplied certificate pair, it generates a self-signed certificate. Non-loopback access requires a token except when `--listen-allow-insecure-remote` explicitly opts out. A configured token remains mandatory even with that flag. Plaintext exposes control traffic and bearer credentials to network observers.

The token-bearing pairing URL is a credential. Do not publish it or include it in screenshots. A client held in `token_required` shows the pairing card rather than opening an unauthenticated retry loop. Saving a nonempty token scopes it to that listener and reconnects.

The separate Agent Client Protocol WebSocket service is loopback-only and does not inherit standalone-listener TLS behavior.

## Markdown, Mermaid, and previews

Web Markdown is **escape-first**: raw user/model HTML is never rendered as document markup. Formatting operates on escaped input, with constrained links and images. Transcript rich text and Markdown file previews share the safe renderer.

Mermaid runs with `securityLevel: "strict"`; its SVG is sanitized before caching/viewing/download. Scripts, embedded foreign content, event-handler attributes, and unsafe URL-bearing markup are not trusted merely because Mermaid produced them.

HTML file preview is intentionally separate: `.html` and `.htm` render in an iframe with `sandbox=""` and no-referrer policy. Scripts are not enabled. SVG files render through an image element, not inline executable SVG. The sandbox is not a claim that preview content cannot reference external resources.

The explorer bounds directory entries and changed-file catalogs at 5,000. The client independently caps changed-file rendering at 5,000. Model-facing search defaults are grep 1,000, ls/find 5,000, and glob 200. Bounds protect rendering and resource use without making ordinary project exploration artificially small.

## Imports and exports

Imports read foreign session content and convert it with agent-loader. OMP chains retain lineage boundaries for members excluded by the 64 MiB per-member limit and deduplicate repeated message identifiers. Foreign content is still untrusted conversation material.

Conversion uses uniquely named temporary directories for staged input and emitted records. On Unix, both paths set directory permissions to owner-only (`0700`) before writing session content. Permission-setting failures stop conversion rather than writing content into an unrestricted staging directory.

HTML export escapes transcript content but can still contain sensitive code, paths, and prose. Secret gists are unlisted, not private. Review exported data before sharing and keep recovery archives protected with the source journal.

## Credentials

Authentication and secret-bearing settings use owner-only persistence. Secret catalog values are redacted in views; do not write those display values back as credentials. Best-effort redaction cannot recognize every sensitive string. Avoid logging full prompts, provider keys, process environments, credential files, and pairing URLs.

## In-flight queue contract

Rust steering queue and scoped cancellation integration are in progress. Web previews do not establish a durable queue or cancellation guarantee. Use the shared **Stop** action for active generation; its wire identifier remains `abort`.
