# Sandbox confinement, Browser proof, and workflow isolation

Sandbox policy, project trust, tool approval, and workflow worktrees address different boundaries. Selecting one does not implicitly enable the others.

## Modes and platforms

| Mode | Boundary |
| --- | --- |
| `read-only` | Confined filesystem policy without ordinary workspace writes |
| `workspace-write` | Confined policy with explicitly writable workspace roots |
| `danger-full-access` | Raw host execution; no confinement backend |

Linux confinement uses a trusted Bubblewrap backend. macOS confinement uses Seatbelt through `sandbox-exec`. Windows rejects confined modes; `danger-full-access` uses host execution. A missing or invalid confinement backend is not silently treated as successful confinement.

Network policy is independently represented as `none`, `proxy-routed`, or `full-access`. Filesystem policy distinguishes read-only roots, writable roots, protected descendants, and sealed scratch authority. Do not interpret a writable workspace as unrestricted network or credential access.

## Validation and authority

The application binds resources, tools, runtime, and security to one generation. Confined execution retains revision-specific filesystem roots; backend preparation rejects missing or inconsistent root authority.

`danger-full-access` cannot be combined with confined policy fields. Overlapping incompatible read-only/writable roots are rejected with configuration diagnostics instead of silently widening access.

Workspace paths are opened through capability authority. Project context files use no-follow reads. An operator-selected directory is validated as a directory and its retained identity is checked before publication; a displayed path alone is not proof of live authority.

CLI `-C` requires an existing directory and never creates one. Session-creation workspace selection has its own operator-directory boundary; do not use startup semantics as a substitute for that contract.

## Browser and process execution

Browser automation is a governed tool: the tool factory requires an application generation rather than constructing a standalone browser with ambient authority. Its process and transport configuration must satisfy that generation's policy. A screenshot proves rendered output, not the confinement boundary itself.

Long-lived shell commands must be supervised with `background=true` or the process tool. Detached shell syntax is rejected. Shell `grep` and `rg` are not blocked by the bash parser; search-tool preference is separate prompt guidance.

## Workflows

An isolated Git worktree separates concurrent modifications. It does not deny network access, remove host credentials, or prove process confinement. Workflow state and lifecycle remain owned by the session that created it.

## Security practice

Inspect effective settings and approval mode before executing unfamiliar project code. Review trust requests, preserve policy errors, and do not replace a failed confined configuration with full access merely to make a tool run. See [Security](security.md) for rendering, imports, and listener authentication.
