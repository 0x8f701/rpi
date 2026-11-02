# Packages

The resource-package backend supports **local directories and Git sources**. It does not accept npm as an installed resource-package backend. The separate plugin marketplace can support npm plugin sources; do not interchange its commands with `rpi install`.

## Commands

```text
rpi install SOURCE
rpi install SOURCE --local
rpi list
rpi config
rpi remove SOURCE
rpi remove SOURCE --local
```

The ordinary scope is global; `--local` selects project configuration. `remove` also has the `uninstall` alias. `rpi config` without a settings subcommand configures enabled package resources.

Resource configuration can select extension, skill, prompt, and theme contributions, with an optional autoload choice. Project package resources remain subject to project trust.

## Source identity and state

A local source records its path. A Git source records repository, host, path, and optional reference. A supplied reference marks a pinned source. Package source, manifest, and state records use schema version 1; unsupported versions are rejected.

Package operations are serialized. Settings and package state are atomically replaced. Git is invoked with an argument vector, not an implicit shell command. Git execution has a timeout and bounded output.

## Updates

`rpi update --extensions` updates configured extension packages. `--extension SOURCE` selects one source; `--all` selects the managed rpi installation and extensions. `rpi update --models` refreshes dynamic model catalogs instead of updating packages.

Marketplace commands live under `rpi plugin`, with installed plugin content under the active agent directory's `extensions` tree. Use the subcommand help for its source and update options.

## Security

Treat package content as external code and instructions. Pin a reviewed Git reference when reproducibility matters. Installation, resource discovery, and permission to execute are separate stages. Do not grant project trust just to silence a missing-resource error.

See [extensions](extensions.md), [skills](skills.md), and [settings/trust](settings-trust.md).
