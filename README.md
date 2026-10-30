# rpi

rpi is a Rust coding agent for interactive and noninteractive coding. It provides a terminal interface, text and JSON output, a JSONL control protocol, a browser client, and Agent Client Protocol integration for editors. Native sessions use Pi v3 JSONL.

## Install

Linux/macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/0x8f701/rpi/master/install.sh | sh
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/0x8f701/rpi/master/install.ps1 | iex
```

The installer selects a release archive and verifies `SHA256SUMS` before activation. Ordinary installation requires no Rust checkout. See [installation](docs/src/introduction/install.md) for platform boundaries and source builds.

## Start

```sh
rpi setup
rpi login
rpi models
rpi
```

Choose an available model and submit a prompt. Other entry points:

```sh
rpi --print "Explain the project structure"
rpi --mode json "List the main modules"
rpi --continue
rpi rpc
rpi --listen 127.0.0.1:8543
rpi agent stdio
```

`-C DIR` selects an existing working directory; CLI startup never creates it. The standalone Web listener uses HTTPS by default and runs independently of standard input. Treat its token-bearing pairing URL as a credential. A token-required client presents a dedicated pairing card.

## Capabilities

- **Coding tools:** file read/write/edit, content and structural search, browser automation, service tools, language/debug adapters, evaluation, and notebooks, subject to active configuration and authority.
- **Supervised work:** long-lived commands use `background=true` or the process tool. Shell `grep`/`rg` are allowed; unsupervised background/detach is rejected.
- **Automation:** recurring prompt loops, with Web presets 30s, 1m, 5m, 15m, 1h, and 4h.
- **Processes:** Web command spawning through `bash -c` in the session working directory, per-process output, Stop escalation, and three-second polling while mounted.
- **Files:** Source/Preview surfaces for safe Markdown, sandboxed HTML, and SVG images; directory and changed-file bounds of 5,000.
- **Orchestration:** child coding jobs, peer messaging, Goals, Todo dependency execution, and worktree-isolated workflows.
- **Sessions:** resume, rewind/checkpoints, compaction, handoff, HTML/current-branch JSONL export, and cross-agent import through agent-loader v0.7.0.
- **Configuration:** profiles, global/project settings, TOML/JSON persistence, scoped credentials, resources, extensions, and packages.

Model-facing search defaults: grep 1,000 matches; ls and find 5,000 entries/results; glob 200. OMP rotation imports merge root-to-leaf with leaf identity/working directory, root start time, and message-identifier deduplication; oversize members remain lineage boundaries while excluded from conversion.

**Steering queue status: in progress.** The Web preview component is implemented; Rust queue and scoped cancellation integration are not yet a stable end-to-end contract.

## Documentation

Start at the [book introduction](docs/src/README.md) or [full contents](docs/src/SUMMARY.md).

- [Quick start](docs/src/introduction/quickstart.md)
- [CLI modes and commands](docs/src/user-guide/cli-modes.md)
- [TUI and keybindings](docs/src/user-guide/tui.md)
- [Web client](docs/src/user-guide/web.md)
- [RPC JSONL](docs/src/user-guide/rpc-json.md)
- [Models](docs/src/user-guide/models.md) and [authentication](docs/src/user-guide/authentication.md)
- [Session recovery/import](docs/src/user-guide/session-recovery.md)
- [Settings and trust](docs/src/reference/settings-trust.md)
- [Architecture](docs/src/reference/architecture.md)
- [Security](docs/src/reference/security.md)

## Security

Safe rendering is not process confinement. Web Markdown escapes raw HTML; Mermaid uses strict sanitization; HTML preview uses an empty-sandbox iframe. Project trust and tool approval remain separate from sandbox mode. Linux/macOS confined modes require their supported backend; Windows does not provide confinement and rejects confined modes.

Protect credentials, session exports, and pairing URLs. Secret gists are unlisted rather than private. Review the [security reference](docs/src/reference/security.md) before enabling remote access or running untrusted resources.

## Development

The workspace contains `pi-ai`, `pi-agent`, `pi-coding`, `pi-cli`, and `pi-sandbox`, with Rust 1.88 as the declared minimum. Maintainer source build:

```sh
cargo +1.88.0 build -p pi-cli --release
```

Follow `AGENTS.md` for shared-worktree and verification rules. This README describes functionality; it does not certify that the current working tree has passed all verification suites.

## License

MIT — see [LICENSE](LICENSE).
