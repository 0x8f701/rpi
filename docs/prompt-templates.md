# Prompt templates and system prompt assembly

This page covers three related things:

- **Prompt templates** — reusable slash-command expansions (`/name`) for the user prompt.
- **Project context files** — `AGENTS.md` / `CLAUDE.md` instructions injected into the system prompt.
- **System prompt assembly** — how the final system prompt is built from the role text, tool snippets, guidelines, context, and skills.

For how skills and agents differ from prompt templates, see [`skills.md`](skills.md).

In this document `<agent-dir>` means the resolved agent configuration directory. The runtime resolves it from the `PI_CODING_AGENT_DIR` environment variable when set, otherwise from the platform default (see [`settings-trust.md`](settings-trust.md)).

## Prompt templates

A prompt template is a Markdown file with optional YAML frontmatter. The file stem is the command name, the frontmatter describes it, and the body is the expansion text.

Templates live in:

- `<agent-dir>/prompts/` — global templates, always discovered.
- `<workspace>/.pi/prompts/` — project-local templates, discovered only when the project is trusted.
- Any path passed with `--prompt-template PATH`.

Settings can also reference named templates from [configured packages](packages.md):

```json
{
  "prompts": ["custom-prompt"]
}
```

### File format

```markdown
---
description: Review code for common issues
argument-hint: "[files...]"
---

Review the following files for bugs, style, and performance:

$ARGUMENTS
```

Fields:

| Field | Purpose |
|-------|---------|
| `description` | Shown in the interactive command list and used for the template table. If omitted, the first non-empty line of the body is truncated to 60 characters with `...`. |
| `argument-hint` | Optional hint shown in command completion. |

The template **name** is the file stem (`review.md` → `/review`). Duplicate names across discovery sources are an error at load time.

### How templates differ from skills, agents, and context files

| Concept | What it is | Where it appears |
|---------|-----------|------------------|
| **Prompt template** | A reusable user-prompt fragment | Expanded into the *user* message when you type `/name` in the REPL or TUI |
| **Skill** | Specialized instructions loaded on demand | Listed in the *system* prompt under `<available_skills>`; the model loads it via `skill://<name>` |
| **Agent** | A subagent definition with its own system prompt | Used by the orchestration runtime to spawn child sessions (see [`skills.md`](skills.md#agents)) |
| **Context file** | Project-specific instructions | Injected verbatim into the *system* prompt under `<project_context>` |

### Discovery order and precedence

The `ResourceManager` loads prompt templates from these sources, in order:

1. Global default directory: `<agent-dir>/prompts/`.
2. Project default directory: `<workspace>/.pi/prompts/` (only when trusted).
3. Settings `prompts` and package `prompts` resources.
4. Explicit `--prompt-template PATH` arguments.

If two loaded templates have the same name, loading fails with a collision error.

### Trust gates

Project-local `.pi/prompts/` are loaded only when the resolved project trust decision allows project resources. Trust is resolved by [`crates/pi-coding/src/trust.rs`](../crates/pi-coding/src/trust.rs) using, in order:

1. The one-run CLI flag `--approve` / `--no-approve`.
2. A persisted decision in `<agent-dir>/trust.json`.
3. `defaultProjectTrust` from settings (`ask`, `always`, `never`).
4. In headless modes (`--print`, `--mode json`, `--mode rpc`), an unset decision is treated as untrusted.

If a project has no `.pi` directory at all, it is trusted by default because there are no project-local resources to gate.

CLI flags:

```sh
pi --prompt-template ./prompts/review.md          # load an explicit template for interactive use
pi --no-prompt-templates --print "hello"          # disables discovered/configured templates, explicit paths still load
```

### Safe path containment

Explicit template paths are canonicalized and validated before loading:

- Relative paths are resolved against `--cwd` and must not escape it through symlinks.
- Paths inside a project `.pi/` directory require project trust.
- Paths that do not exist are a hard error.

This is implemented in `validate_explicit_project_paths` in [`crates/pi-coding/src/resource_manager.rs`](../crates/pi-coding/src/resource_manager.rs).

### Template expansion syntax

In the REPL or TUI, type `/name arg1 arg2` to expand a template. Expansion is performed by `expand_prompt_template` / `substitute_args` in [`crates/pi-coding/src/prompt_templates.rs`](../crates/pi-coding/src/prompt_templates.rs).

Supported placeholders:

| Placeholder | Meaning |
|-------------|---------|
| `$1`, `$2`, … | Positional argument (1-based). Missing arguments expand to empty. |
| `$@`, `$ARGUMENTS` | All arguments joined by a single space. |
| `${n:-default}` | Positional argument `n` or `default` if empty. |
| `${@:-default}` | All arguments or `default` if empty. |
| `${@:start:length}` | Slice of the argument list (1-based `start`, optional `length`). |

Important properties:

- Arguments are parsed with the same quoting rules as the original Go implementation: unquoted whitespace splits arguments; single or double quotes group arguments.
- Substitution is a **single regex pass**. Values inserted by a substitution are never scanned again, so a literal `$1` in an argument or default stays literal.
- If no template matches the `/name` command, the original text is returned unchanged.

### Example: create and use a template

```sh
mkdir -p "$PI_CODING_AGENT_DIR/prompts"
cat > "$PI_CODING_AGENT_DIR/prompts/review.md" <<'EOF'
---
description: Review code for common issues
argument-hint: "[files...]"
---

Review the following files for bugs, style, and performance:

$ARGUMENTS
EOF
```

In an interactive `pi` session:

```
/review src/main.rs src/lib.rs
```

That expands to the template body with `$ARGUMENTS` replaced by `src/main.rs src/lib.rs`.

## Project context files

The CLI discovers `AGENTS.md`, `AGENTS.MD`, `CLAUDE.md`, and `CLAUDE.MD` in two places:

1. The agent config directory (`<agent-dir>`).
2. Each ancestor directory of the working directory, from the filesystem root down to `--cwd`.

Global context is always loaded. Ancestor/project context is loaded only when the project is trusted (see [Trust gates](#trust-gates)). The CLI skips git-worktree-shadowed files so the same tracked context file is not loaded twice.

Context files are wrapped as:

```xml
<project_context>

Project-specific instructions and guidelines:

<project_instructions path="AGENTS.md">
...
</project_instructions>

</project_context>
```

Disable context-file discovery with `--no-context-files`.

## System prompt files

In addition to context files, the CLI can load full system-prompt overrides from:

- `<agent-dir>/SYSTEM.md`
- `<workspace>/.pi/SYSTEM.md` (project, trusted)

And append-only blocks from:

- `<agent-dir>/APPEND_SYSTEM.md`
- `<workspace>/.pi/APPEND_SYSTEM.md` (project, trusted)

CLI flags take precedence:

- `--system-prompt TEXT_OR_PATH` (or `--system`) replaces the default role section.
- `--append-system-prompt TEXT_OR_PATH` appends text after the role section.

When a path is given to `--system-prompt` or `--append-system-prompt`, it is canonicalized and a relative path that escapes `--cwd` through symlinks is rejected.

## System prompt assembly

The final system prompt is assembled by `build_system_prompt` in [`crates/pi-coding/src/system_prompt.rs`](../crates/pi-coding/src/system_prompt.rs) using the active `ResourceSnapshot` from the `ResourceManager`.

With the default role section, the prompt contains:

1. The role description.
2. `Available tools:` — one-line snippets for each selected tool.
3. `Guidelines:` — de-duplicated prompt guidelines from built-in tools and `prompt_guidelines`, plus:
   - `Use bash for file operations like ls, rg, find` when `bash` is present without `grep`/`find`/`ls`.
   - `Be concise in your responses`.
   - `Show file paths clearly when working with files`.
4. A block pointing to the pi docs and examples paths.
5. The `--append-system-prompt` text, if any.
6. The `<project_context>` block, if any context files are loaded.
7. The `<available_skills>` block, **only when the `read` tool is selected**.
8. `Current working directory: <cwd>`.

When `--system-prompt` is provided, the custom text replaces the default role section, tool list, guidelines, and pi documentation block. Any `--append-system-prompt`, `<project_context>`, and `<available_skills>` blocks are still appended.

The `read` tool supports `skill://<name>` and `skill://<name>/<relative-path>` paths. The skill resolver in [`crates/pi-coding/src/selector.rs`](../crates/pi-coding/src/selector.rs) confines access to the skill's base directory.

## Programmatic use and SDK behavior

The low-level `Session::new` defaults to trusted-project resource discovery. The higher-level `AgentSessionBuilder` in [`crates/pi-coding/src/sdk.rs`](../crates/pi-coding/src/sdk.rs) defaults to **no discovery**:

```rust
use pi_ai::Model;
use pi_coding::{AgentSessionBuilder, AgentSession, ResourceDiscovery};

async fn no_discovery_session(model: Model) -> Result<AgentSession, Box<dyn std::error::Error>> {
    let session = AgentSessionBuilder::new(model, "<workspace>")
        .resource_discovery(ResourceDiscovery::Disabled) // default
        .build()
        .await?;
    Ok(session)
}
```

To opt into discovery, call `discover_resources(ResourceManagerOptions)`. It still requires project trust for project-local resources:

```rust
use pi_ai::Model;
use pi_coding::{AgentSessionBuilder, AgentSession, ResourceManagerOptions};

async fn discovery_session(model: Model) -> Result<AgentSession, Box<dyn std::error::Error>> {
    let session = AgentSessionBuilder::new(model, "<workspace>")
        .discover_resources(ResourceManagerOptions::new("<workspace>", "<agent-dir>"))?
        .build()
        .await?;
    Ok(session)
}
```

This explicit, opt-in design means SDK consumers do not accidentally load project instructions, skills, or prompts.

You can also build a system prompt directly without any discovery:

```rust
use pi_coding::{build_system_prompt, BuildSystemPromptOptions, ContextFile, Skill, SkillSource};
use std::collections::HashMap;

let mut snippets = HashMap::new();
snippets.insert("read".into(), "Read file contents".into());

let prompt = build_system_prompt(BuildSystemPromptOptions {
    custom_prompt: "You are a Rust reviewer.".into(),
    selected_tools: vec!["read".into()],
    tool_snippets: snippets,
    cwd: "<workspace>".into(),
    context_files: vec![ContextFile {
        path: "AGENTS.md".into(),
        content: "Be concise.".into(),
    }],
    skills: vec![Skill {
        name: "rust-review".into(),
        description: "Review Rust code for idioms and safety.".into(),
        file_path: "<workspace>/.pi/skills/rust-review/SKILL.md".into(),
        base_dir: "<workspace>/.pi/skills/rust-review".into(),
        globs: vec!["*.rs".into()],
        always_apply: false,
        hidden: false,
        disable_model_invocation: false,
        source: SkillSource::Project,
        trusted: true,
    }],
    ..BuildSystemPromptOptions::default()
});
```

See also [`skills.md`](skills.md) for skill frontmatter, selection, and resolver details.
