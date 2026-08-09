# Packages

`rpi` supports local-directory and git-based packages. The npm backend is
deliberately deferred and cannot be installed.

Source: `crates/pi-coding/src/packages.rs:1-5` and
`crates/pi-coding/src/packages.rs:842-845`.

## Package sources

Valid package sources:

- A plain filesystem path, relative to the current working directory or
  absolute. The stored identity becomes `local:<canonical path>`.
  - Example: `./my-pi-tools`
- A git URL using `https`, `http`, `ssh`, or `git` as the scheme.
  - Examples: `https://github.com/owner/repo`,
    `git@github.com:owner/repo`, `ssh://git@github.com/owner/repo`,
    `git:https://github.com/owner/repo`
- A `git:` shorthand using a host with a domain or `localhost`:
  `git:github.com/owner/repo`. The shorthand must include host, owner, and
  repository.
- A git ref pinned with `@ref`: `git:github.com/owner/repo@v1.2`.
- `npm:package[@version]` — **not implemented**; rejected with a clear error.

Source: `crates/pi-coding/src/packages.rs:810-903`.

## Install, remove, list, config, update

```sh
# Install globally (stored in agent settings)
rpi install git:github.com/owner/pi-my-tools

# Install into project settings (requires project trust)
rpi install ./my-pi-tools --local

# Remove a configured package
rpi remove git:github.com/owner/pi-my-tools

# List configured packages and their install status
rpi list

# Toggle enabled package resources for global or project scope
rpi config
rpi config -l            # project scope; requires project trust

# Update rpi itself (default when no package flags are given)
rpi update
rpi update --self

# Update every configured package (also accepts --all)
rpi update --extensions
rpi update --all

# Update one configured package by source identity
rpi update git:github.com/owner/pi-my-tools
rpi update github.com/owner/pi-my-tools

# Update packages and rpi itself
rpi update --self --extensions

# Reinstall self-update even when version and checksum match
rpi update --self --force
```

Source: `crates/pi-cli/src/args.rs:244-285` and
`crates/pi-cli/src/lib.rs:62-120`.

`rpi list` shows the scope (`global` or `project`), source, status
(`installed`, `missing`, or `unsupported`), whether a git source is pinned to
a ref, and the on-disk path for installed packages.

Source: `crates/pi-cli/src/package_commands.rs:34-66` and
`crates/pi-coding/src/packages.rs:777-800`.

`rpi config` discovers resources declared by each package's
`package.json#pi` manifest and lets you enable or disable individual extensions,
skills, prompts, and themes. In a headless environment (stdout is not a TTY) it
prints deterministic JSON and never blocks. Project scope is refused when the
project is not trusted.

Source: `crates/pi-cli/src/package_config.rs:1-16` and
`crates/pi-cli/src/package_config.rs:542-550`.

`rpi update --extensions` reconciles every configured git/local package. Pinned
git refs are fetched and reset to the configured ref; unpinned sources follow the
remote default branch. `npm:` entries are skipped. `rpi update PACKAGE` reconciles
one configured package by identity; matching `npm:` sources produce the deferred
error.

Source: `crates/pi-coding/src/packages.rs:495-545`.

Git packages are cloned into a content-addressed directory under the scope root
(`<agent-dir>/git/...` for global packages, `<workspace>/.pi/git/...` for
project packages). Local packages are referenced by path. Atomic checkout
swapping, serialized operations, and atomic settings/state writes prevent
partial installs.

Source: `crates/pi-coding/src/packages.rs:57-77`,
`crates/pi-coding/src/packages.rs:408-530`,
`crates/pi-coding/src/packages.rs:711-770`, and
`crates/pi-coding/src/packages.rs:1867-1954`.

## `settings.json` packages field

The `packages` array holds either a source string or a filtered object:

```json
{
  "packages": [
    "git:github.com/owner/pi-my-tools",
    {
      "source": "git:github.com/owner/pi-skills",
      "autoload": true,
      "extensions": ["pi-my-ext"],
      "skills": ["rust-review"],
      "prompts": ["custom-prompt"],
      "themes": ["solarized"]
    }
  ]
}
```

Source: `crates/pi-coding/src/settings.rs:34-45`.

- `autoload: true` enables all resources from the package by default.
- `extensions`, `skills`, `prompts`, `themes` are resource-filter lists. They
  may contain exact resource names, glob patterns, `!` exclusions, and `+` / `-`
  force-include/exclude tokens used by `rpi config`.
- Project entries win over global entries with the same package identity.

Source: `crates/pi-coding/src/packages.rs:524-550` and
`crates/pi-cli/src/package_config.rs:1-16`.

## Package manifest (`package.json#pi`)

A package root may contain a `package.json` whose `pi` field declares its
resources:

```json
{
  "name": "pi-my-tools",
  "pi": {
    "schemaVersion": 1,
    "extensions": ["extensions/*"],
    "skills": ["skills/**/*.md"],
    "prompts": ["prompts/**/*.md"],
    "themes": ["themes/*.json"]
  }
}
```

Source: `crates/pi-coding/src/packages.rs:139-165`.

Without a manifest, the package manager discovers resources under standard
subdirectories:

- `extensions/` — files named `pi-extension.json`.
- `skills/` — `SKILL.md` or `.md` files.
- `prompts/` — `.md` files.
- `themes/` — `.json` files.

Hidden entries, symlinks, and `node_modules` are ignored during discovery.

Source: `crates/pi-coding/src/packages.rs:1303-1453`.

## Trust and scope

- Global packages are always loaded.
- Project packages are loaded only when the project is trusted.
- Project resources win over global resources with the same name.

Source: `crates/pi-coding/src/packages.rs:524-550`.

## Plugin marketplace (`rpi plugin`)

Separate from `rpi install` packages, the **plugin marketplace**
(`crates/pi-coding/src/plugin.rs`) installs standalone extension packages —
a directory whose root carries a validated `pi-extension.json` manifest (the
same schema the extension runtime loads). Installed plugins land in
`<agent_dir>/extensions/<name>/` and are picked up by the resource scan, so
an installed plugin is loadable through the normal extension pipeline.

```sh
rpi plugin list                   # name, version, runtime, trust state
rpi plugin list --updates         # check the marketplace index for updates
rpi plugin install <SOURCE>       # install and record explicit Trusted consent
rpi plugin remove <NAME>          # remove and clear the stored trust decision
rpi plugin update <NAME>          # re-stage from the index entry, atomic swap
```

`install`/`update` accept:

- a local directory,
- a local or remote `.tgz` / `.tar.gz` / `.tar` archive,
- an `owner/repo` GitHub reference,
- an `npm:<name>[@<version>]` reference — resolved through the npm registry
  to the package's `dist.tarball`, with the tarball's content authenticated
  against `dist.integrity` (`sha512-<base64>`; any other algorithm, a
  missing integrity field, or a digest mismatch fails closed),
- a git URL (`git+https://host/owner/repo`, `git+ssh://git@host/owner/repo.git`,
  `https://host/owner/repo.git`, `ssh://git@host/owner/repo.git`,
  `git@host:owner/repo.git`).

Bounded everywhere: 256 MiB package cap, 1 MiB manifest cap, 2 MiB npm
metadata cap, 60 s HTTP fetch timeout, 120 s git-clone timeout, and
URL-credential redaction in every error.

Trust model: `install` records the user's explicit consent as a `Trusted`
decision for the plugin path; `remove` clears it. A plugin whose canonical
path resolves to anything other than `Trusted` is listed but **not** loaded
by `marketplace_extension_resources`, so untrusted plugins never execute.
`update` preserves the stored trust decision.

The marketplace index is a JSON list of `{name, repo, version, description?}`
entries fetched from the `pluginMarketplace` setting (a URL or a local index
file) or the embedded default. `npm:` sources are supported here even though
the `rpi install` package manager defers them.

## What is not supported

- `npm:` sources (for the `rpi install` package manager; the plugin
  marketplace accepts them via `rpi plugin install`).
- `package.json` lifecycle hooks or registry metadata.
- A `local:` scheme prefix. Local packages are written as plain filesystem
  paths.
