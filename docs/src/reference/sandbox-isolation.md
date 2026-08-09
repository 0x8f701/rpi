# Sandbox and overlayfs isolation

This page documents the two Linux confinement/isolation layers: the
**filesystem sandbox** for process spawns (bash, process extensions, and
orchestration subagents) and the **overlayfs isolation** backend used by
workflow checkouts.

## Filesystem sandbox

The sandbox (`crates/pi-coding/src/sandbox.rs`) is an opt-in Linux filesystem
sandbox for process spawns. It is **confinement, not isolation**: the command
still runs as the same user with the same privileges, but inside fresh Linux
namespaces (`unshare`) so that

- only the configured allowed paths are visible (bind-mounted read-write, or
  read-only when `sandbox.readOnly` is set),
- everything else on the host filesystem is denied (tmpfs root +
  `pivot_root`),
- the command gets a private, empty `HOME` and `TMPDIR` under the sandbox
  root instead of the host home (codex/claude parity),
- the network is off by default (fresh net namespace, loopback only),
- `/proc` reflects only the sandbox's own PID namespace.

System binaries and libraries under `/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`
are bind-mounted read-only so commands can execute; user data under those
roots is still hidden. Denied paths are overlaid with an empty tmpfs mount so
they are invisible even when nested inside an allowed path.

Requirements: Linux and the `unshare` command (util-linux). Unprivileged
users additionally need user namespaces (`kernel.unprivileged_userns_clone`
or equivalent); the wrapper maps the caller to root *inside* the namespaces
only — there is no privilege escalation on the host. On non-Linux targets
every sandbox entry point returns an explicit "sandbox unsupported on this
platform" error.

### Settings

```json
{
  "sandbox": {
    "enabled": true,
    "network": false,
    "readOnly": false,
    "allowedPaths": ["<workspace>", "<data-root>"],
    "deniedPaths": ["<workspace>/secrets"]
  }
}
```

| Key | Default | Meaning |
|-----|---------|---------|
| `enabled` | (off) | Master switch for sandboxed spawns. |
| `network` | `false` | Share the host network; default is a fresh net namespace with loopback only. |
| `readOnly` | `false` | Bind allowed paths read-only. |
| `allowedPaths` | `[cwd, agent_dir]` | Paths visible inside the sandbox. Relative paths resolve from `cwd`. |
| `deniedPaths` | (none) | Paths overlaid with an empty tmpfs (invisible even inside allowed paths). |

`deniedPaths` wins over `allowedPaths` (denied overlays come last in the
mount order). The working directory must live inside an allowed path,
otherwise its inode would be detached after `pivot_root`.

### What the sandbox covers

- **bash tool** — when `settings.sandbox.enabled` is set, `bash` commands run
  through `run_in_sandbox` (streaming merged output, timeout, abort; the
  whole process group is killed on either so namespaced descendants cannot
  linger).
- **Process extensions** — long-lived protocol children can be spawned with
  `spawn_piped` (same fail-closed validation and allowed/denied path
  semantics).
- **Orchestration subagents** — with `settings.orchestration.sandboxed =
  true`, every process a child spawns (its bash tool) is confined to the
  workspace, the agent directory, and `sandbox.allowedPaths`
  (`settings.rs:318-325`).

The environment inside the sandbox is fully controlled by the caller (env is
applied after `env_clear`); the sandbox path lists are appended last so a
host environment can never spoof them.

### Invariants

- Every sandboxed spawn validates its configuration fail-closed before the
  `unshare` wrapper is constructed.
- Setup is PID 1 of the fresh namespaces and fails closed: every step exits
  with a distinct code and an actionable message on stderr.
- The sandbox never escalates privileges: the caller is mapped to root only
  inside the user namespace, and the host uid is unchanged.

## Overlayfs isolation

`crates/pi-coding/src/isolate.rs` provides an overlayfs isolation backend
(OMP pi-iso parity): a writable "merged" view over a read-only "lower" tree
without a deep copy. `OverlayfsIsolation::start` materializes `merged` as the
union of `lower` (read-only) and `upper` (private, writable copy-on-write);
`stop` detaches the mount (`umount -l`) and cleans the upper/work
directories.

Backend fallback chain, tried in order until one succeeds
(`OverlayBackend`):

1. **Kernel overlay** — `mount -t overlay -o lowerdir=,upperdir=,workdir=`.
   Requires mount privilege: real root, or a user namespace that owns the
   process mount namespace.
2. **fuse-overlayfs** — PATH lookup; runs as a FUSE daemon, works
   unprivileged and is visible to every process.
3. **rcopy** — recursive copy of `lower` into `merged`; no mount needed.

Backends never degrade silently: the first candidate that succeeds wins and
every later candidate is skipped; the copy fallback guarantees callers always
get a usable writable view. The chosen backend is serialized by the workflow
isolation manager so a restored workflow re-establishes exactly the backend
it had before a restart (rcopy must never be re-run over an existing upper,
which would clobber the workflow's changes). Timeouts bound hung mounts: 30 s
per mount command, 5 s fuse readiness.

### Use in workflows

`settings.orchestration.isolation = "overlayfs"` selects the overlayfs
workflow backend (`workflow_worktree/overlay.rs`): each workflow gets a
private overlay whose read-only lower layer is the live source tree and whose
writable upper layer is a private per-workflow dir. Integration commits the
upper state as a single commit on the source branch — no merge history, no
merge-conflict detection; overlayfs integration is last-writer-wins by
design, and `Conflicted` is never produced by this backend. See
[`workflows.md`](../user-guide/workflows.md) for the isolation options
(`worktree` default, `overlayfs`, `none`).

## Related documentation

- [`security.md`](security.md) — path containment and process ownership
  hardening
- [`workflows.md`](../user-guide/workflows.md) — workflow isolation backends
- [`settings-trust.md`](settings-trust.md) — `sandbox` and
  `orchestration.sandboxed` settings
