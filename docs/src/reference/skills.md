# Skills

Skills are file-backed instructions discovered as resources and presented to the model. They are not executable tool registrations and do not grant filesystem authority.

## Discovery

A directory containing `SKILL.md` is a skill root; discovery stops descending beneath that root. Otherwise the loader accepts direct Markdown children and searches subdirectories for skill roots. Ignore files and excluded dependency trees constrain traversal, and resolved targets are deduplicated.

Skills carry a name, description, file path, base directory, and matching metadata. Discovery emits diagnostics for invalid resources rather than making malformed files valid by omitting their metadata.

Use `--skill PATH` for an explicit file or directory; the option is repeatable. `--no-skills` disables configured/discovered skills while retaining explicit paths. Project-origin resources remain subject to their trust and capability boundary.

## Prompt use

The system prompt presents available skills when the selected tools include `read`. The model reads a skill's instruction file when relevant; listing the skill does not inject every instruction body or automatically execute a command.

Relative resource references belong to the skill's base directory. A path mentioned in skill text is still just text: accessing it requires the tool's workspace or resource authority.

## Context files are separate

`AGENTS.md` and `CLAUDE.md` are project context files, not skill roots. Context discovery uses the project boundary and has strict no-follow rules for project files. `--no-context-files` disables that discovery independently of skill selection.

The project's frontend design rule calls for generous user-facing limits. Current model search defaults are grep 1,000, ls 5,000, find 5,000, and glob 200; see [tools](tools.md). This policy does not make skill or context loading unbounded.

## Safety

Read imported skills as untrusted instructions until their origin is reviewed. Skills cannot override host approval, sandbox confinement, or project trust. Use [packages](packages.md) for distribution and [prompt assembly](prompt-templates.md) for how resources reach a session.
